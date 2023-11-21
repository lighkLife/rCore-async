///! Ref: https://www.lammertbies.nl/comm/info/serial-uart
///! Ref: ns16550a datasheet: https://datasheetspdf.com/pdf-file/605590/NationalSemiconductor/NS16550A/1
///! Ref: ns16450 datasheet: https://datasheetspdf.com/pdf-file/1311818/NationalSemiconductor/NS16450/1
use super::CharDevice;
use crate::sync::{Condvar, UPIntrFreeCell};
use crate::task::schedule;
use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use core::task::Poll::{Pending, Ready};
use bitflags::*;
use volatile::{ReadOnly, Volatile, WriteOnly};
use crate::board::irq_handler;

bitflags! {
    /// InterruptEnableRegister
    pub struct IER: u8 {
        const RX_AVAILABLE = 1 << 0;
        const TX_EMPTY = 1 << 1;
    }

    /// LineStatusRegister
    pub struct LSR: u8 {
        const DATA_AVAILABLE = 1 << 0;
        const THR_EMPTY = 1 << 5;
    }

    /// Model Control Register
    pub struct MCR: u8 {
        const DATA_TERMINAL_READY = 1 << 0;
        const REQUEST_TO_SEND = 1 << 1;
        const AUX_OUTPUT1 = 1 << 2;
        const AUX_OUTPUT2 = 1 << 3;
    }
}

#[repr(C)]
#[allow(dead_code)]
struct ReadWithoutDLAB {
    /// receiver buffer register
    pub rbr: ReadOnly<u8>,
    /// interrupt enable register
    pub ier: Volatile<IER>,
    /// interrupt identification register
    pub iir: ReadOnly<u8>,
    /// line control register
    pub lcr: Volatile<u8>,
    /// model control register
    pub mcr: Volatile<MCR>,
    /// line status register
    pub lsr: ReadOnly<LSR>,
    /// ignore MSR
    _padding1: ReadOnly<u8>,
    /// ignore SCR
    _padding2: ReadOnly<u8>,
}

#[repr(C)]
#[allow(dead_code)]
struct WriteWithoutDLAB {
    /// transmitter holding register
    pub thr: WriteOnly<u8>,
    /// interrupt enable register
    pub ier: Volatile<IER>,
    /// ignore FCR
    _padding0: ReadOnly<u8>,
    /// line control register
    pub lcr: Volatile<u8>,
    /// modem control register
    pub mcr: Volatile<MCR>,
    /// line status register
    pub lsr: ReadOnly<LSR>,
    /// ignore other registers
    _padding1: ReadOnly<u16>,
}

pub struct NS16550aRaw {
    base_addr: usize,
}

impl NS16550aRaw {
    fn read_end(&mut self) -> &mut ReadWithoutDLAB {
        unsafe { &mut *(self.base_addr as *mut ReadWithoutDLAB) }
    }

    fn write_end(&mut self) -> &mut WriteWithoutDLAB {
        unsafe { &mut *(self.base_addr as *mut WriteWithoutDLAB) }
    }

    pub fn new(base_addr: usize) -> Self {
        Self { base_addr }
    }

    pub fn init(&mut self) {
        let read_end = self.read_end();
        let mut mcr = MCR::empty();
        mcr |= MCR::DATA_TERMINAL_READY;
        mcr |= MCR::REQUEST_TO_SEND;
        mcr |= MCR::AUX_OUTPUT2;
        read_end.mcr.write(mcr);
        let ier = IER::RX_AVAILABLE;
        read_end.ier.write(ier);
    }

    pub fn read(&mut self) -> Option<u8> {
        let read_end = self.read_end();
        let lsr = read_end.lsr.read();
        if lsr.contains(LSR::DATA_AVAILABLE) {
            Some(read_end.rbr.read())
        } else {
            None
        }
    }

    pub async fn write(&mut self, ch: u8) {
        let write_end = self.write_end();
        loop {
            if write_end.lsr.read().contains(LSR::THR_EMPTY) {
                write_end.thr.write(ch);
                break;
            }
        }
    }
}


struct NS16550aInner {
    ns16550a: NS16550aRaw,
    read_buffer: VecDeque<u8>,
}


pub struct NS16550a<const BASE_ADDR: usize> {
    inner: UPIntrFreeCell<NS16550aInner>,
    waker_list: VecDeque<Waker>,
}

impl<const BASE_ADDR: usize> NS16550a<BASE_ADDR> {
    pub fn new() -> Self {
        let inner = NS16550aInner {
            ns16550a: NS16550aRaw::new(BASE_ADDR),
            read_buffer: VecDeque::new(),
        };
        //inner.ns16550a.init();
        Self {
            inner: unsafe { UPIntrFreeCell::new(inner) },
            waker_list: VecDeque::new(),
        }
    }

    pub fn read_buffer_is_empty(&self) -> bool {
        self.inner
            .exclusive_session(|inner| inner.read_buffer.is_empty())
    }
}

impl<const BASE_ADDR: usize> CharDevice for NS16550a<BASE_ADDR> {
    fn init(&self) {
        let mut inner = self.inner.exclusive_access();
        inner.ns16550a.init();
        drop(inner);
    }

    fn read(&self) -> u8 {
        loop {
            let mut inner = self.inner.exclusive_access();
            if let Some(ch) = inner.read_buffer.pop_front() {
                return ch;
            } else {
                let task_cx_ptr = self.condvar.wait_no_sched();
                drop(inner);
                schedule(task_cx_ptr);
            }
        }
    }
    async fn write(&self, ch: u8) {
        let mut inner = self.inner.exclusive_access();
        inner.ns16550a.write(ch).await;
    }

    fn handle_irq(&mut self) {
        self.inner.exclusive_session(|inner| {
            if let Some(ch) = inner.ns16550a.read() {
                inner.read_buffer.push_back(ch);
            }
            if let Some(waker) = self.waker_list.pop() {
                waker.clone().wake();
            }
        });
    }
}

struct AsyncCharWriter<const BASE_ADDR: usize> {
    uart: NS16550a<BASE_ADDR>,
    waker_list: VecDeque<Waker>,
}

impl<const BASE_ADDR: usize> Future for AsyncCharWriter<BASE_ADDR> {
    type Output = ();

    fn poll(&mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let uart = self.uart.inner.exclusive_access();
        let write_end = uart.ns16550a.write_end();
        if write_end.lsr.read().contains(LSR::THR_EMPTY) {
            // writable
            Ready()
        } else {
            let waker = cx.waker().clone();
            self.waker_list.push_back(waker);
            Pending
        }
    }
}
