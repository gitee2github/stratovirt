// Copyright (c) 2023 Huawei Technologies Co.,Ltd. All rights reserved.
//
// StratoVirt is licensed under Mulan PSL v2.
// You can use this software according to the terms and conditions of the Mulan
// PSL v2.
// You may obtain a copy of Mulan PSL v2 at:
//         http://license.coscl.org.cn/MulanPSL2
// THIS SOFTWARE IS PROVIDED ON AN "AS IS" BASIS, WITHOUT WARRANTIES OF ANY
// KIND, EITHER EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO
// NON-INFRINGEMENT, MERCHANTABILITY OR FIT FOR A PARTICULAR PURPOSE.
// See the Mulan PSL v2 for more details.

use std::{
    cell::RefCell,
    fs::File,
    io::{Seek, SeekFrom},
    os::unix::prelude::{AsRawFd, RawFd},
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use anyhow::{Context, Result};
use log::error;
use vmm_sys_util::epoll::EventSet;

use crate::{BlockIoErrorCallback, BlockProperty};
use machine_manager::event_loop::{register_event_helper, unregister_event_helper};
use util::{
    aio::{get_iov_size, Aio, AioCb, AioEngine, Iovec, OpCode},
    loop_context::{
        read_fd, EventNotifier, EventNotifierHelper, NotifierCallback, NotifierOperation,
    },
};

pub struct FileDriver<T: Clone + 'static> {
    file: File,
    aio: Rc<RefCell<Aio<T>>>,
    incomplete: Arc<AtomicU64>,
    delete_evts: Vec<RawFd>,
    block_prop: BlockProperty,
}

impl<T: Clone + 'static> FileDriver<T> {
    pub fn new(file: File, aio: Aio<T>, block_prop: BlockProperty) -> Self {
        Self {
            file,
            incomplete: aio.incomplete_cnt.clone(),
            aio: Rc::new(RefCell::new(aio)),
            delete_evts: Vec::new(),
            block_prop,
        }
    }

    fn package_aiocb(
        &self,
        opcode: OpCode,
        iovec: Vec<Iovec>,
        offset: usize,
        nbytes: u64,
        iocompletecb: T,
    ) -> AioCb<T> {
        AioCb {
            direct: self.block_prop.direct,
            req_align: self.block_prop.req_align,
            buf_align: self.block_prop.buf_align,
            file_fd: self.file.as_raw_fd(),
            opcode,
            iovec,
            offset,
            nbytes,
            user_data: 0,
            iocompletecb,
            discard: self.block_prop.discard,
            write_zeroes: self.block_prop.write_zeroes,
        }
    }

    pub fn read_vectored(&mut self, iovec: &[Iovec], offset: usize, completecb: T) -> Result<()> {
        let aiocb = self.package_aiocb(
            OpCode::Preadv,
            iovec.to_vec(),
            offset,
            get_iov_size(iovec),
            completecb,
        );
        self.aio.borrow_mut().submit_request(aiocb)
    }

    pub fn write_vectored(&mut self, iovec: &[Iovec], offset: usize, completecb: T) -> Result<()> {
        let aiocb = self.package_aiocb(
            OpCode::Pwritev,
            iovec.to_vec(),
            offset,
            get_iov_size(iovec),
            completecb,
        );
        self.aio.borrow_mut().submit_request(aiocb)
    }

    pub fn write_zeroes(
        &mut self,
        offset: usize,
        nbytes: u64,
        completecb: T,
        unmap: bool,
    ) -> Result<()> {
        let opcode = if unmap {
            OpCode::WriteZeroesUnmap
        } else {
            OpCode::WriteZeroes
        };
        let aiocb = self.package_aiocb(opcode, Vec::new(), offset, nbytes, completecb);
        self.aio.borrow_mut().submit_request(aiocb)
    }

    pub fn discard(&mut self, offset: usize, nbytes: u64, completecb: T) -> Result<()> {
        let aiocb = self.package_aiocb(OpCode::Discard, Vec::new(), offset, nbytes, completecb);
        self.aio.borrow_mut().submit_request(aiocb)
    }

    pub fn datasync(&mut self, completecb: T) -> Result<()> {
        let aiocb = self.package_aiocb(OpCode::Fdsync, Vec::new(), 0, 0, completecb);
        self.aio.borrow_mut().submit_request(aiocb)
    }

    pub fn flush_request(&mut self) -> Result<()> {
        self.aio.borrow_mut().flush_request()
    }

    pub fn drain_request(&self) {
        while self.incomplete.load(Ordering::Acquire) != 0 {
            continue;
        }
    }

    pub fn register_io_event(
        &mut self,
        broken: Arc<AtomicBool>,
        error_cb: BlockIoErrorCallback,
    ) -> Result<()> {
        let handler = FileIoHandler::new(self.aio.clone(), broken, error_cb);
        let notifiers = EventNotifierHelper::internal_notifiers(Arc::new(Mutex::new(handler)));
        register_event_helper(
            notifiers,
            self.block_prop.iothread.as_ref(),
            &mut self.delete_evts,
        )
    }

    pub fn unregister_io_event(&mut self) -> Result<()> {
        unregister_event_helper(self.block_prop.iothread.as_ref(), &mut self.delete_evts)
    }

    pub fn disk_size(&mut self) -> Result<u64> {
        let disk_size = self
            .file
            .seek(SeekFrom::End(0))
            .with_context(|| "Failed to seek the end for file")?;
        Ok(disk_size)
    }
}

struct FileIoHandler<T: Clone + 'static> {
    aio: Rc<RefCell<Aio<T>>>,
    broken: Arc<AtomicBool>,
    error_cb: BlockIoErrorCallback,
}

impl<T: Clone + 'static> FileIoHandler<T> {
    pub fn new(
        aio: Rc<RefCell<Aio<T>>>,
        broken: Arc<AtomicBool>,
        error_cb: BlockIoErrorCallback,
    ) -> Self {
        Self {
            aio,
            broken,
            error_cb,
        }
    }

    fn aio_complete_handler(&mut self) -> Result<bool> {
        let error_cb = self.error_cb.clone();
        self.aio.borrow_mut().handle_complete().map_err(|e| {
            error_cb();
            e
        })
    }
}

impl<T: Clone + 'static> EventNotifierHelper for FileIoHandler<T> {
    fn internal_notifiers(handler: Arc<Mutex<Self>>) -> Vec<EventNotifier> {
        let handler_raw = handler.lock().unwrap();
        let mut notifiers = Vec::new();

        // Register event notifier for aio.
        let h_clone = handler.clone();
        let h: Rc<NotifierCallback> = Rc::new(move |_, fd: RawFd| {
            read_fd(fd);
            let mut h_lock = h_clone.lock().unwrap();
            if h_lock.broken.load(Ordering::SeqCst) {
                return None;
            }
            if let Err(ref e) = h_lock.aio_complete_handler() {
                error!("Failed to handle aio {:?}", e);
            }
            None
        });
        let h_clone = handler.clone();
        let handler_iopoll: Box<NotifierCallback> = Box::new(move |_, _fd: RawFd| {
            let mut h_lock = h_clone.lock().unwrap();
            if h_lock.broken.load(Ordering::SeqCst) {
                return None;
            }
            if h_lock.aio.borrow_mut().get_engine() == AioEngine::Off {
                return None;
            }
            match h_lock.aio_complete_handler() {
                Ok(done) => {
                    if done {
                        Some(Vec::new())
                    } else {
                        None
                    }
                }
                Err(e) => {
                    error!("Failed to handle aio {:?}", e);
                    None
                }
            }
        });
        notifiers.push(build_event_notifier(
            handler_raw.aio.borrow_mut().fd.as_raw_fd(),
            vec![h],
            Some(handler_iopoll),
        ));

        notifiers
    }
}

fn build_event_notifier(
    fd: RawFd,
    handlers: Vec<Rc<NotifierCallback>>,
    handler_poll: Option<Box<NotifierCallback>>,
) -> EventNotifier {
    let mut notifier = EventNotifier::new(
        NotifierOperation::AddShared,
        fd,
        None,
        EventSet::IN,
        handlers,
    );
    notifier.handler_poll = handler_poll;
    notifier
}