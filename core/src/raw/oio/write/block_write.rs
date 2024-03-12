// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use std::pin::Pin;
use std::sync::Arc;
use std::task::ready;
use std::task::Context;
use std::task::Poll;

use async_trait::async_trait;
use futures::Future;
use futures::FutureExt;
use futures::StreamExt;
use uuid::Uuid;

use crate::raw::*;
use crate::*;

/// BlockWrite is used to implement [`Write`] based on block
/// uploads. By implementing BlockWrite, services don't need to
/// care about the details of uploading blocks.
///
/// # Architecture
///
/// The architecture after adopting [`BlockWrite`]:
///
/// - Services impl `BlockWrite`
/// - `BlockWriter` impl `Write`
/// - Expose `BlockWriter` as `Accessor::Writer`
///
/// # Notes
///
/// `BlockWrite` has an oneshot optimization when `write` has been called only once:
///
/// ```no_build
/// w.write(bs).await?;
/// w.close().await?;
/// ```
///
/// We will use `write_once` instead of starting a new block upload.
///
/// # Requirements
///
/// Services that implement `BlockWrite` must fulfill the following requirements:
///
/// - Must be a http service that could accept `AsyncBody`.
/// - Don't need initialization before writing.
/// - Block ID is generated by caller `BlockWrite` instead of services.
/// - Complete block by an ordered block id list.
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
pub trait BlockWrite: Send + Sync + Unpin + 'static {
    /// write_once is used to write the data to underlying storage at once.
    ///
    /// BlockWriter will call this API when:
    ///
    /// - All the data has been written to the buffer and we can perform the upload at once.
    async fn write_once(&self, size: u64, body: AsyncBody) -> Result<()>;

    /// write_block will write a block of the data and returns the result
    /// [`Block`].
    ///
    /// BlockWriter will call this API and stores the result in
    /// order.
    ///
    /// - block_id is the id of the block.
    async fn write_block(&self, block_id: Uuid, size: u64, body: AsyncBody) -> Result<()>;

    /// complete_block will complete the block upload to build the final
    /// file.
    async fn complete_block(&self, block_ids: Vec<Uuid>) -> Result<()>;

    /// abort_block will cancel the block upload and purge all data.
    async fn abort_block(&self, block_ids: Vec<Uuid>) -> Result<()>;
}

/// WriteBlockResult is the result returned by [`WriteBlockFuture`].
///
/// The error part will carries input `(block_id, bytes, err)` so caller can retry them.
type WriteBlockResult = std::result::Result<Uuid, (Uuid, oio::ChunkedBytes, Error)>;

struct WriteBlockFuture(BoxedStaticFuture<WriteBlockResult>);

/// # Safety
///
/// wasm32 is a special target that we only have one event-loop for this WriteBlockFuture.
unsafe impl Send for WriteBlockFuture {}

/// # Safety
///
/// We will only take `&mut Self` reference for WriteBlockFuture.
unsafe impl Sync for WriteBlockFuture {}

impl Future for WriteBlockFuture {
    type Output = WriteBlockResult;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().0.poll_unpin(cx)
    }
}

impl WriteBlockFuture {
    pub fn new<W: BlockWrite>(w: Arc<W>, block_id: Uuid, bytes: oio::ChunkedBytes) -> Self {
        let fut = async move {
            w.write_block(
                block_id,
                bytes.len() as u64,
                AsyncBody::ChunkedBytes(bytes.clone()),
            )
            .await
            // Return bytes while we got an error to allow retry.
            .map_err(|err| (block_id, bytes, err))
            // Return the successful block id.
            .map(|_| block_id)
        };

        WriteBlockFuture(Box::pin(fut))
    }
}

/// BlockWriter will implements [`Write`] based on block
/// uploads.
pub struct BlockWriter<W: BlockWrite> {
    state: State,
    w: Arc<W>,

    block_ids: Vec<Uuid>,
    cache: Option<oio::ChunkedBytes>,
    futures: ConcurrentFutures<WriteBlockFuture>,
}

enum State {
    Idle,
    Close(BoxedStaticFuture<Result<()>>),
    Abort(BoxedStaticFuture<Result<()>>),
}

/// # Safety
///
/// wasm32 is a special target that we only have one event-loop for this state.
unsafe impl Send for State {}
/// # Safety
///
/// We will only take `&mut Self` reference for State.
unsafe impl Sync for State {}

impl<W: BlockWrite> BlockWriter<W> {
    /// Create a new BlockWriter.
    pub fn new(inner: W, concurrent: usize) -> Self {
        Self {
            state: State::Idle,

            w: Arc::new(inner),
            block_ids: Vec::new(),
            cache: None,
            futures: ConcurrentFutures::new(1.max(concurrent)),
        }
    }

    fn fill_cache(&mut self, bs: &dyn oio::WriteBuf) -> usize {
        let size = bs.remaining();
        let bs = oio::ChunkedBytes::from_vec(bs.vectored_bytes(size));
        assert!(self.cache.is_none());
        self.cache = Some(bs);
        size
    }
}

impl<W> oio::Write for BlockWriter<W>
where
    W: BlockWrite,
{
    fn poll_write(&mut self, cx: &mut Context<'_>, bs: &dyn oio::WriteBuf) -> Poll<Result<usize>> {
        loop {
            match &mut self.state {
                State::Idle => {
                    if self.futures.has_remaining() {
                        // Fill cache with the first write.
                        if self.cache.is_none() {
                            let size = self.fill_cache(bs);
                            return Poll::Ready(Ok(size));
                        }

                        let cache = self.cache.take().expect("pending write must exist");
                        self.futures.push_back(WriteBlockFuture::new(
                            self.w.clone(),
                            Uuid::new_v4(),
                            cache,
                        ));

                        let size = self.fill_cache(bs);
                        return Poll::Ready(Ok(size));
                    } else if let Some(res) = ready!(self.futures.poll_next_unpin(cx)) {
                        match res {
                            Ok(block_id) => {
                                self.block_ids.push(block_id);
                            }
                            Err((block_id, bytes, err)) => {
                                self.futures.push_front(WriteBlockFuture::new(
                                    self.w.clone(),
                                    block_id,
                                    bytes,
                                ));
                                return Poll::Ready(Err(err));
                            }
                        }
                    }
                }
                State::Close(_) => {
                    unreachable!("BlockWriter must not go into State::Close during poll_write")
                }
                State::Abort(_) => {
                    unreachable!("BlockWriter must not go into State::Abort during poll_write")
                }
            }
        }
    }

    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<Result<()>> {
        loop {
            match &mut self.state {
                State::Idle => {
                    // No write block has been sent.
                    if self.futures.is_empty() && self.block_ids.is_empty() {
                        let w = self.w.clone();
                        let (size, body) = match self.cache.clone() {
                            Some(cache) => (cache.len(), AsyncBody::ChunkedBytes(cache)),
                            None => (0, AsyncBody::Empty),
                        };
                        // Call write_once if there is no data in buffer and no location.
                        self.state =
                            State::Close(Box::pin(
                                async move { w.write_once(size as u64, body).await },
                            ));
                        continue;
                    }

                    if self.futures.has_remaining() {
                        if let Some(cache) = self.cache.take() {
                            self.futures.push_back(WriteBlockFuture::new(
                                self.w.clone(),
                                Uuid::new_v4(),
                                cache,
                            ));
                        }
                    }

                    if !self.futures.is_empty() {
                        while let Some(result) = ready!(self.futures.poll_next_unpin(cx)) {
                            match result {
                                Ok(block_id) => {
                                    self.block_ids.push(block_id);
                                }
                                Err((block_id, bytes, err)) => {
                                    self.futures.push_front(WriteBlockFuture::new(
                                        self.w.clone(),
                                        block_id,
                                        bytes,
                                    ));
                                    return Poll::Ready(Err(err));
                                }
                            }
                        }
                    } else {
                        let w = self.w.clone();
                        let block_ids = self.block_ids.clone();
                        self.state =
                            State::Close(Box::pin(
                                async move { w.complete_block(block_ids).await },
                            ));
                        continue;
                    }
                }
                State::Close(fut) => {
                    let res = futures::ready!(fut.as_mut().poll(cx));
                    self.state = State::Idle;
                    // We should check res first before clean up cache.
                    res?;
                    self.cache = None;

                    return Poll::Ready(Ok(()));
                }
                State::Abort(_) => {
                    unreachable!("BlockWriter must not go into State::Abort during poll_close")
                }
            }
        }
    }

    fn poll_abort(&mut self, cx: &mut Context<'_>) -> Poll<Result<()>> {
        loop {
            match &mut self.state {
                State::Idle => {
                    let w = self.w.clone();
                    let block_ids = self.block_ids.clone();
                    self.futures.clear();
                    self.cache = None;
                    self.state =
                        State::Abort(Box::pin(async move { w.abort_block(block_ids).await }));
                }
                State::Abort(fut) => {
                    let res = futures::ready!(fut.as_mut().poll(cx));
                    self.state = State::Idle;
                    return Poll::Ready(res);
                }
                State::Close(_) => {
                    unreachable!("BlockWriter must not go into State::Close during poll_abort")
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use bytes::Bytes;
    use pretty_assertions::assert_eq;
    use rand::thread_rng;
    use rand::Rng;
    use rand::RngCore;

    use super::*;
    use crate::raw::oio::StreamExt;
    use crate::raw::oio::WriteBuf;
    use crate::raw::oio::WriteExt;

    struct TestWrite {
        length: u64,
        bytes: HashMap<Uuid, Bytes>,
        content: Option<Bytes>,
    }

    impl TestWrite {
        pub fn new() -> Arc<Mutex<Self>> {
            let v = Self {
                length: 0,
                bytes: HashMap::new(),
                content: None,
            };

            Arc::new(Mutex::new(v))
        }
    }

    #[cfg_attr(not(target_arch = "wasm32"), async_trait)]
    #[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
    impl BlockWrite for Arc<Mutex<TestWrite>> {
        async fn write_once(&self, _: u64, _: AsyncBody) -> Result<()> {
            Ok(())
        }

        async fn write_block(&self, block_id: Uuid, size: u64, body: AsyncBody) -> Result<()> {
            // We will have 50% percent rate for write part to fail.
            if thread_rng().gen_bool(5.0 / 10.0) {
                return Err(Error::new(ErrorKind::Unexpected, "I'm a crazy monkey!"));
            }

            let bs = match body {
                AsyncBody::Empty => Bytes::new(),
                AsyncBody::Bytes(bs) => bs,
                AsyncBody::ChunkedBytes(cb) => cb.bytes(cb.remaining()),
                AsyncBody::Stream(s) => s.collect().await.unwrap(),
            };

            let mut this = self.lock().unwrap();
            this.length += size;
            this.bytes.insert(block_id, bs);

            Ok(())
        }

        async fn complete_block(&self, block_ids: Vec<Uuid>) -> Result<()> {
            let mut this = self.lock().unwrap();
            let mut bs = Vec::new();
            for id in block_ids {
                bs.extend_from_slice(&this.bytes[&id]);
            }
            this.content = Some(bs.into());

            Ok(())
        }

        async fn abort_block(&self, _: Vec<Uuid>) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_block_writer_with_concurrent_errors() {
        let mut rng = thread_rng();

        let mut w = BlockWriter::new(TestWrite::new(), 8);
        let mut total_size = 0u64;
        let mut expected_content = Vec::new();

        for _ in 0..1000 {
            let size = rng.gen_range(1..1024);
            total_size += size as u64;

            let mut bs = vec![0; size];
            rng.fill_bytes(&mut bs);

            expected_content.extend_from_slice(&bs);

            loop {
                match w.write(&bs.as_slice()).await {
                    Ok(_) => break,
                    Err(_) => continue,
                }
            }
        }

        loop {
            match w.close().await {
                Ok(_) => break,
                Err(_) => continue,
            }
        }

        let inner = w.w.lock().unwrap();

        assert_eq!(total_size, inner.length, "length must be the same");
        assert!(inner.content.is_some());
        assert_eq!(
            expected_content,
            inner.content.clone().unwrap(),
            "content must be the same"
        );
    }
}
