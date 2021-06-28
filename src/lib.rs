mod builder;
mod envelop;
pub mod error;
mod handler;
mod receiver;
pub mod receivers;
mod trait_object;

#[macro_use]
extern crate log;

use crate::receiver::Permit;
pub use builder::BusBuilder;
use core::any::{Any, TypeId};
pub use envelop::Message;
pub use handler::*;
use receiver::Receiver;
use smallvec::SmallVec;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};
use tokio::sync::oneshot;
use error::{Error, SendError, StdSyncSendError};

pub type Untyped = Arc<dyn Any + Send + Sync>;

static ID_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SendOptions {
    Broadcast,
    Direct(u64),
    Random,
    Balanced,
}

impl Default for SendOptions {
    fn default() -> Self {
        Self::Broadcast
    }
}

pub struct BusInner {
    receivers: HashMap<TypeId, SmallVec<[Receiver; 4]>>,
    closed: AtomicBool,
}

impl BusInner {
    pub(crate) fn new(input: Vec<(TypeId, Receiver)>) -> Self {
        let mut receivers = HashMap::new();

        for (key, value) in input {
            receivers
                .entry(key)
                .or_insert_with(SmallVec::new)
                .push(value);
        }

        Self {
            receivers,
            closed: AtomicBool::new(false),
        }
    }

    pub async fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);

        for (_, rs) in &self.receivers {
            for r in rs {
                r.close().await;
            }
        }
    }

    pub async fn flush(&self) {
        let fuse_count = 32i32;
        let mut breaked = false;
        let mut iters = 0usize;
        for _ in 0..fuse_count {
            iters += 1;
            let mut flushed = false;
            for (_, rs) in &self.receivers {
                for r in rs {
                    if r.need_flush() {
                        flushed = true;
                        r.flush().await;
                    }
                }
            }

            if !flushed {
                breaked = true;
                break;
            }
        }

        if !breaked {
            warn!(
                "!!! WARNING: unable to reach equilibrium in {} iterations !!!",
                fuse_count
            );
        } else {
            info!("flushed in {} iterations !!!", iters);
        }
    }

    pub async fn flush_and_sync(&self) {
        self.flush().await;

        for (_, rs) in &self.receivers {
            for r in rs {
                r.sync().await;
            }
        }
    }

    fn try_reserve(&self, rs: &[Receiver]) -> Option<SmallVec<[Permit; 32]>> {
        let mut permits = SmallVec::<[Permit; 32]>::new();

        for r in rs {
            if let Some(prmt) = r.try_reserve() {
                permits.push(prmt);
            } else {
                return None;
            };
        }

        Some(permits)
    }

    #[inline]
    pub fn try_send<M: Message>(&self, msg: M) -> core::result::Result<(), SendError<M>> {
        self.try_send_ext(msg, SendOptions::Broadcast)
    }

    pub fn try_send_ext<M: Message>(
        &self,
        msg: M,
        _options: SendOptions,
    ) -> core::result::Result<(), SendError<M>> {
        if self.closed.load(Ordering::SeqCst) {
            warn!("Bus closed. Skipping send!");
            return Ok(());
        }

        let mid = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tid = TypeId::of::<M>();

        if let Some(rs) = self.receivers.get(&tid) {
            let permits = if let Some(x) = self.try_reserve(rs) {
                x
            } else {
                return Err(SendError::Full(msg));
            };

            let mut iter = permits.into_iter().zip(rs.iter());
            let mut counter = 1;
            let total = rs.len();

            while counter < total {
                let (p, r) = iter.next().unwrap();
                let _ = r.send(mid, p, msg.clone());

                counter += 1;
            }

            if let Some((p, r)) = iter.next() {
                let _ = r.send(mid, p, msg);
                return Ok(());
            }
        }

        warn!(
            "Unhandled message {:?}: no receivers",
            core::any::type_name::<M>()
        );

        Ok(())
    }

    #[inline]
    pub fn send_blocking<M: Message>(&self, msg: M) -> core::result::Result<(), SendError<M>> {
        self.send_blocking_ext(msg, SendOptions::Broadcast)
    }

    #[inline]
    pub fn send_blocking_ext<M: Message>(
        &self,
        msg: M,
        options: SendOptions,
    ) -> core::result::Result<(), SendError<M>> {
        futures::executor::block_on(self.send_ext(msg, options))
    }

    #[inline]
    pub async fn send<M: Message>(&self, msg: M) -> core::result::Result<(), Error<M>> {
        Ok(self.send_ext(msg, SendOptions::Broadcast).await?)
    }

    pub async fn send_ext<M: Message>(
        &self,
        msg: M,
        _options: SendOptions,
    ) -> core::result::Result<(), SendError<M>> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(SendError::Closed(msg));
        }

        let mid = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tid = TypeId::of::<M>();

        if let Some(rs) = self.receivers.get(&tid) {
            if let Some((last, head)) = rs.split_last() {
                for r in head {
                    let _ = r.send(mid, r.reserve().await, msg.clone());
                }

                let _ = last.send(mid, last.reserve().await, msg);

                return Ok(());
            }
        }

        warn!(
            "Unhandled message {:?}: no receivers",
            core::any::type_name::<M>()
        );

        Ok(())
    }

    #[inline]
    pub fn force_send<M: Message>(&self, msg: M) -> core::result::Result<(), SendError<M>> {
        self.force_send_ext(msg, SendOptions::Broadcast)
    }

    pub fn force_send_ext<M: Message>(
        &self,
        msg: M,
        _options: SendOptions,
    ) -> core::result::Result<(), SendError<M>> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(SendError::Closed(msg));
        }

        let mid = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tid = TypeId::of::<M>();

        if let Some(rs) = self.receivers.get(&tid) {
            if let Some((last, head)) = rs.split_last() {
                for r in head {
                    let _ = r.force_send(mid, msg.clone());
                }

                let _ = last.force_send(mid, msg);

                return Ok(());
            }
        }

        warn!(
            "Unhandled message {:?}: no receivers",
            core::any::type_name::<M>()
        );

        Ok(())
    }

    pub async fn request<M: Message, R: Message>(
        &self,
        req: M,
        options: SendOptions,
    ) -> Result<R, Error<M>> {
        let tid = TypeId::of::<M>();
        let rid = TypeId::of::<R>();

        let mut iter = self.select_receivers(tid, options, Some(rid), None);
        if let Some(rc) = iter.next() {
            let (tx, rx) = oneshot::channel();
            let mid = (rc.add_response_waiter(tx).unwrap() | 1 << (usize::BITS - 1)) as u64;
            rc.send(mid, rc.reserve().await, req)?;

            rx.await?.map_err(|x|x.specify::<M>())
        } else {
            Err(Error::NoReceivers)
        }
    }

    pub async fn request_we<M, R, E>(
        &self,
        req: M,
        options: SendOptions,
    ) -> Result<R, Error<M, E>> 
        where 
            M: Message,
            R: Message,
            E: StdSyncSendError
    {
        let tid = TypeId::of::<M>();
        let rid = TypeId::of::<R>();
        let eid = TypeId::of::<E>();

        let mut iter = self.select_receivers(tid, options, Some(rid), Some(eid));
        if let Some(rc) = iter.next() {
            let (tx, rx) = oneshot::channel();
            let mid = (rc.add_response_waiter_we(tx).unwrap() | 1 << (usize::BITS - 1)) as u64;
            rc.send(mid, rc.reserve().await, req)?;

            rx.await?
                .map_err(|x|x.specify::<M>())
        } else {
            Err(Error::NoReceivers)
        }
    }

    #[inline]
    fn select_receivers(
        &self,
        tid: TypeId,
        _options: SendOptions,
        rid: Option<TypeId>,
        eid: Option<TypeId>,
    ) -> impl Iterator<Item = &Receiver> + '_ {
        self.receivers
            .get(&tid)
            .into_iter()
            .map(|item| item.iter())
            .flatten()
            .filter(move |x| match (rid, eid) {
                (Some(r), Some(e)) => x.resp_type_id() == r && x.err_type_id() == e,
                (Some(r), None) => x.resp_type_id() == r,
                (None, Some(e)) => x.err_type_id() == e,
                (None, None) => true,
            })
    }
}

#[derive(Clone)]
pub struct Bus {
    inner: Arc<BusInner>,
}

impl core::ops::Deref for Bus {
    type Target = BusInner;

    fn deref(&self) -> &Self::Target {
        self.inner.as_ref()
    }
}

impl Bus {
    #[inline]
    pub fn build() -> BusBuilder {
        BusBuilder::new()
    }
}
