use super::{
    address::VAddr,
    gdt::{USER_CS64, USER_DS},
    syscall::SyscallFrame,
    tss::TSS,
    SpinLockGuard, UserVAddr, KERNEL_STACK_SIZE,
};
use super::{cpu_local::cpu_local_head, gdt::USER_RPL};
use crate::mm::page_allocator::{alloc_pages, AllocPageFlags};
use crate::result::Result;
use x86::current::segmentation::wrfsbase;

#[repr(C, packed)]
pub struct Thread {
    rsp: u64,
    pub(super) fsbase: u64,
    pub(super) xsave_area: Option<VAddr>,
    interrupt_stack: VAddr,
    syscall_stack: VAddr,
}

extern "C" {
    fn kthread_entry();
    fn userland_entry();
    fn forked_child_entry();
    fn signal_handler_entry();
    fn do_switch_thread(prev_rsp: *const u64, next_rsp: *const u64);
}

unsafe fn push_stack(mut rsp: *mut u64, value: u64) -> *mut u64 {
    rsp = rsp.sub(1);
    rsp.write(value);
    rsp
}

impl Thread {
    #[allow(unused)]
    pub fn new_kthread(ip: VAddr, sp: VAddr) -> Thread {
        let interrupt_stack = alloc_pages(1, AllocPageFlags::KERNEL)
            .expect("failed to allocate kernel stack")
            .as_vaddr();
        let syscall_stack = alloc_pages(1, AllocPageFlags::KERNEL)
            .expect("failed to allocate kernel stack")
            .as_vaddr();

        let rsp = unsafe {
            let mut rsp: *mut u64 = sp.as_mut_ptr();

            // Registers to be restored in kthread_entry().
            rsp = push_stack(rsp, ip.value() as u64); // The entry point.

            // Registers to be restored in do_switch_thread().
            rsp = push_stack(rsp, kthread_entry as *const u8 as u64); // RIP.
            rsp = push_stack(rsp, 0); // Initial RBP.
            rsp = push_stack(rsp, 0); // Initial RBX.
            rsp = push_stack(rsp, 0); // Initial R12.
            rsp = push_stack(rsp, 0); // Initial R13.
            rsp = push_stack(rsp, 0); // Initial R14.
            rsp = push_stack(rsp, 0); // Initial R15.
            rsp = push_stack(rsp, 0x02); // RFLAGS (interrupts disabled).

            rsp
        };

        Thread {
            rsp: rsp as u64,
            fsbase: 0,
            xsave_area: None,
            interrupt_stack,
            syscall_stack,
        }
    }

    pub fn new_user_thread(ip: UserVAddr, sp: UserVAddr, kernel_sp: VAddr) -> Thread {
        let interrupt_stack = alloc_pages(1, AllocPageFlags::KERNEL)
            .expect("failed to allocate kernel stack")
            .as_vaddr();
        let syscall_stack = alloc_pages(1, AllocPageFlags::KERNEL)
            .expect("failed to allocate kernel stack")
            .as_vaddr();
        // TODO: Check the size of XSAVE area.
        let xsave_area = alloc_pages(1, AllocPageFlags::KERNEL)
            .expect("failed to allocate xsave area")
            .as_vaddr();

        let rsp = unsafe {
            let mut rsp: *mut u64 = kernel_sp.as_mut_ptr();

            // Registers to be restored by IRET.
            rsp = push_stack(rsp, (USER_DS | USER_RPL) as u64); // SS
            rsp = push_stack(rsp, sp.value() as u64); // user RSP
            rsp = push_stack(rsp, 0x202); // RFLAGS (interrupts enabled).
            rsp = push_stack(rsp, (USER_CS64 | USER_RPL) as u64); // CS
            rsp = push_stack(rsp, ip.value() as u64); // RIP

            // Registers to be restored in do_switch_thread().
            rsp = push_stack(rsp, userland_entry as *const u8 as u64); // RIP.
            rsp = push_stack(rsp, 0); // Initial RBP.
            rsp = push_stack(rsp, 0); // Initial RBX.
            rsp = push_stack(rsp, 0); // Initial R12.
            rsp = push_stack(rsp, 0); // Initial R13.
            rsp = push_stack(rsp, 0); // Initial R14.
            rsp = push_stack(rsp, 0); // Initial R15.
            rsp = push_stack(rsp, 0x02); // RFLAGS (interrupts disabled).

            rsp
        };

        Thread {
            rsp: rsp as u64,
            fsbase: 0,
            xsave_area: Some(xsave_area),
            interrupt_stack,
            syscall_stack,
        }
    }

    pub fn new_idle_thread() -> Thread {
        let interrupt_stack = alloc_pages(1, AllocPageFlags::KERNEL)
            .expect("failed to allocate kernel stack")
            .as_vaddr();
        let syscall_stack = alloc_pages(1, AllocPageFlags::KERNEL)
            .expect("failed to allocate kernel stack")
            .as_vaddr();

        Thread {
            rsp: 0,
            fsbase: 0,
            xsave_area: None,
            interrupt_stack,
            syscall_stack,
        }
    }

    pub fn fork(&self, frame: &SyscallFrame) -> Result<Thread> {
        // TODO: Check the size of XSAVE area.
        let xsave_area = alloc_pages(1, AllocPageFlags::KERNEL)
            .expect("failed to allocate xsave area")
            .as_vaddr();

        let rsp = unsafe {
            let kernel_sp =
                alloc_pages(1, AllocPageFlags::KERNEL).expect("failed allocate kernel stack");
            let mut rsp: *mut u64 = kernel_sp.as_mut_ptr();

            // Registers to be restored by IRET.
            rsp = push_stack(rsp, (USER_DS | USER_RPL) as u64); // SS
            rsp = push_stack(rsp, frame.rsp); // user RSP
            rsp = push_stack(rsp, frame.rflags); // user RFLAGS.
            rsp = push_stack(rsp, (USER_CS64 | USER_RPL) as u64); // CS
            rsp = push_stack(rsp, frame.rip); // user RIP

            // Registers to be restored in forked_child_entry,
            rsp = push_stack(rsp, frame.rflags); // user R11
            rsp = push_stack(rsp, frame.rip); // user RCX
            rsp = push_stack(rsp, frame.r10);
            rsp = push_stack(rsp, frame.r9);
            rsp = push_stack(rsp, frame.r8);
            rsp = push_stack(rsp, frame.rsi);
            rsp = push_stack(rsp, frame.rdi);
            rsp = push_stack(rsp, frame.rdx);

            // Registers to be restored in do_switch_thread().
            rsp = push_stack(rsp, forked_child_entry as *const u8 as u64); // RIP.
            rsp = push_stack(rsp, frame.rbp); // UserRBP.
            rsp = push_stack(rsp, frame.rbx); // UserRBX.
            rsp = push_stack(rsp, frame.r12); // UserR12.
            rsp = push_stack(rsp, frame.r13); // UserR13.
            rsp = push_stack(rsp, frame.r14); // UserR14.
            rsp = push_stack(rsp, frame.r15); // UserR15.
            rsp = push_stack(rsp, 0x02); // RFLAGS (interrupts disabled).

            rsp
        };

        let interrupt_stack = alloc_pages(1, AllocPageFlags::KERNEL)
            .expect("failed allocate kernel stack")
            .as_vaddr();
        let syscall_stack = alloc_pages(1, AllocPageFlags::KERNEL)
            .expect("failed allocate kernel stack")
            .as_vaddr();

        Ok(Thread {
            rsp: rsp as u64,
            fsbase: self.fsbase,
            xsave_area: Some(xsave_area),
            interrupt_stack,
            syscall_stack,
        })
    }

    pub(super) unsafe fn set_signal_entry(
        mut this: SpinLockGuard<'_, Thread>,
        user_rip: u64,
        user_rsp: u64,
        arg1: u64,
        arg2: u64,
        arg3: u64,
        is_current_process: bool,
    ) {
        let mut tmp = [0u64; 8];
        let mut rsp = if is_current_process {
            tmp.as_mut_ptr().add(tmp.len())
        } else {
            this.rsp as *mut u64
        };

        // Registers to be restored in signal_handler_entry().
        rsp = push_stack(rsp, user_rsp); // User RSP.
        rsp = push_stack(rsp, user_rip); // User RIP.
        rsp = push_stack(rsp, 0x202); // User RFLAGS (interrupts enabled).
        rsp = push_stack(rsp, arg1); // User RDI.
        rsp = push_stack(rsp, arg2); // User RSI.
        rsp = push_stack(rsp, arg3); // User RDX.

        if is_current_process {
            // Resume the user process directly from the signal handler.
            drop(this);
            asm!("mov rsp, rax; jmp direct_signal_handler_entry", in("rax") rsp);
        } else {
            // Registers to be restored in do_switch_thread().
            rsp = push_stack(rsp, signal_handler_entry as *const u8 as u64); // RIP.
            rsp = push_stack(rsp, 0); // Initial RBP.
            rsp = push_stack(rsp, 0); // Initial RBX.
            rsp = push_stack(rsp, 0); // Initial R12.
            rsp = push_stack(rsp, 0); // Initial R13.
            rsp = push_stack(rsp, 0); // Initial R14.
            rsp = push_stack(rsp, 0); // Initial R15.
            rsp = push_stack(rsp, 0x02); // RFLAGS (interrupts disabled).

            this.rsp = rsp as u64;
        }
    }
}

pub fn switch_thread(prev: &mut Thread, next: &mut Thread) {
    let head = cpu_local_head();

    // Switch the kernel stack.
    head.rsp0 = (next.syscall_stack.value() + KERNEL_STACK_SIZE) as u64;
    TSS.as_mut()
        .set_rsp0((next.interrupt_stack.value() + KERNEL_STACK_SIZE) as u64);

    // Save and restore the XSAVE area (i.e. XMM/YMM registrers).
    unsafe {
        use core::arch::x86_64::{_xrstor64, _xsave64};

        let xsave_mask = x86::controlregs::xcr0().bits();
        if let Some(xsave_area) = prev.xsave_area.as_ref() {
            _xsave64(xsave_area.as_mut_ptr(), xsave_mask);
        }
        if let Some(xsave_area) = next.xsave_area.as_ref() {
            _xrstor64(xsave_area.as_mut_ptr(), xsave_mask);
        }
    }

    // Fill an invalid value for now: must be initialized in interrupt handlers.
    head.rsp3 = 0xbaad_5a5a_5b5b_baad;

    unsafe {
        wrfsbase(next.fsbase);
        do_switch_thread(&mut prev.rsp as *mut u64, &mut next.rsp as *mut u64);
    }
}
