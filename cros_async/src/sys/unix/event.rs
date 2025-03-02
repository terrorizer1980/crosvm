// Copyright 2022 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use base::Event;

#[cfg(test)]
use super::FdExecutor;
#[cfg(test)]
use super::URingExecutor;
use crate::AsyncResult;
use crate::EventAsync;
use crate::Executor;

impl EventAsync {
    pub fn new(event: Event, ex: &Executor) -> AsyncResult<EventAsync> {
        ex.async_from(event)
            .map(|io_source| EventAsync { io_source })
    }

    /// Gets the next value from the eventfd.
    pub async fn next_val(&self) -> AsyncResult<u64> {
        self.io_source.read_u64().await
    }

    #[cfg(test)]
    pub(crate) fn new_poll(event: Event, ex: &FdExecutor) -> AsyncResult<EventAsync> {
        super::executor::async_poll_from(event, ex).map(|io_source| EventAsync { io_source })
    }

    #[cfg(test)]
    pub(crate) fn new_uring(event: Event, ex: &URingExecutor) -> AsyncResult<EventAsync> {
        super::executor::async_uring_from(event, ex).map(|io_source| EventAsync { io_source })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::unix::uring_executor::is_uring_stable;

    #[test]
    fn next_val_reads_value() {
        async fn go(event: Event, ex: &Executor) -> u64 {
            let event_async = EventAsync::new(event, ex).unwrap();
            event_async.next_val().await.unwrap()
        }

        let eventfd = Event::new().unwrap();
        eventfd.write(0xaa).unwrap();
        let ex = Executor::new().unwrap();
        let val = ex.run_until(go(eventfd, &ex)).unwrap();
        assert_eq!(val, 0xaa);
    }

    #[test]
    fn next_val_reads_value_poll_and_ring() {
        if !is_uring_stable() {
            return;
        }

        async fn go(event_async: EventAsync) -> u64 {
            event_async.next_val().await.unwrap()
        }

        let eventfd = Event::new().unwrap();
        eventfd.write(0xaa).unwrap();
        let uring_ex = URingExecutor::new().unwrap();
        let val = uring_ex
            .run_until(go(EventAsync::new_uring(eventfd, &uring_ex).unwrap()))
            .unwrap();
        assert_eq!(val, 0xaa);

        let eventfd = Event::new().unwrap();
        eventfd.write(0xaa).unwrap();
        let poll_ex = FdExecutor::new().unwrap();
        let val = poll_ex
            .run_until(go(EventAsync::new_poll(eventfd, &poll_ex).unwrap()))
            .unwrap();
        assert_eq!(val, 0xaa);
    }
}
