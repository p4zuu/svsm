// SPDX-License-Identifier: (GPL-2.0-or-later OR MIT)
//
// Copyright (c) 2022 SUSE LLC
//
// Author: Joerg Roedel <jroedel@suse.de>
//
// vim: ts=4 sw=4 et

use crate::cpu::msr::{write_msr, SEV_GHCB};
use crate::io::IOPort;
use crate::mm::pagetable::get_init_pgtable_locked;
use crate::cpu::flush_tlb_global_sync;
use crate::sev::sev_snp_enabled;
use crate::types::{PhysAddr, VirtAddr, PAGE_SIZE, PAGE_SIZE_2M};
use crate::utils::{is_aligned};
use crate::mm::virt_to_phys;
use crate::mm::validate::{valid_bitmap_clear_valid_4k, valid_bitmap_set_valid_4k, valid_bitmap_valid_addr};
use core::arch::asm;
use core::cell::RefCell;
use core::{mem, ptr};

use super::msr_protocol::{
    invalidate_page_msr, register_ghcb_gpa_msr, request_termination_msr, validate_page_msr,
};
use super::pvalidate;

// TODO: Fix this when Rust gets decent compile time struct offset support
const OFF_CPL: u16 = 0xcb;
const OFF_XSS: u16 = 0x140;
const OFF_DR7: u16 = 0x160;
const OFF_RAX: u16 = 0x1f8;
const OFF_RCX: u16 = 0x308;
const OFF_RDX: u16 = 0x310;
const OFF_RBX: u16 = 0x318;
const OFF_SW_EXIT_CODE: u16 = 0x390;
const OFF_SW_EXIT_INFO_1: u16 = 0x398;
const OFF_SW_EXIT_INFO_2: u16 = 0x3a0;
const OFF_SW_SCRATCH: u16 = 0x3a8;
const OFF_XCR0: u16 = 0x3e8;
const OFF_VALID_BITMAP: u16 = 0x3f0;
const OFF_X87_STATE_GPA: u16 = 0x400;
const _OFF_BUFFER: u16 = 0x800;
const OFF_VERSION: u16 = 0xffa;
const OFF_USAGE: u16 = 0xffc;

#[repr(C, packed)]
pub struct PageStateChangeHeader {
    cur_entry: u16,
    end_entry: u16,
    reserved: u32,
}

pub enum PageStateChangeOp {
    PscPrivate,
    PscShared,
    PscPsmash,
    PscUnsmash,
}

const PSC_GFN_MASK: u64 = ((1u64 << 52) - 1) & !0xfffu64;

const PSC_OP_SHIFT: u8 = 52;
const PSC_OP_PRIVATE: u64 = 1 << PSC_OP_SHIFT;
const PSC_OP_SHARED: u64 = 2 << PSC_OP_SHIFT;
const PSC_OP_PSMASH: u64 = 3 << PSC_OP_SHIFT;
const PSC_OP_UNSMASH: u64 = 4 << PSC_OP_SHIFT;

const PSC_FLAG_HUGE_SHIFT: u8 = 56;
const PSC_FLAG_HUGE: u64 = 1 << PSC_FLAG_HUGE_SHIFT;

const GHCB_BUFFER_SIZE: usize = 0x7f0;

#[repr(C, packed)]
pub struct GHCB {
    reserved_1: [u8; 0xcb],
    cpl: u8,
    reserved_2: [u8; 0x74],
    xss: u64,
    reserved_3: [u8; 0x18],
    dr7: u64,
    reserved_4: [u8; 0x90],
    rax: u64,
    reserved_5: [u8; 0x100],
    reserved_6: u64,
    rcx: u64,
    rdx: u64,
    rbx: u64,
    reserved_7: [u8; 0x70],
    sw_exit_code: u64,
    sw_exit_info_1: u64,
    sw_exit_info_2: u64,
    sw_scratch: u64,
    reserved_8: [u8; 0x38],
    xcr0: u64,
    valid_bitmap: [u64; 2],
    x87_state_gpa: u64,
    reserved_9: [u8; 0x3f8],
    buffer: [u8; GHCB_BUFFER_SIZE],
    reserved_10: [u8; 0xa],
    version: u16,
    usage: u32,
}

#[non_exhaustive]
enum GHCBExitCode {}

impl GHCBExitCode {
    pub const IOIO: u64 = 0x7b;
    pub const SNP_PSC: u64 = 0x8000_0010;
    pub const AP_CREATE: u64 = 0x80000013;
    pub const RUN_VMPL: u64 = 0x80000018;
}

pub enum GHCBIOSize {
    Size8,
    Size16,
    Size32,
}

impl GHCB {
    pub fn init(&mut self) -> Result<(), ()> {
        let vaddr = (self as *const GHCB) as VirtAddr;
        let paddr = virt_to_phys(vaddr);

        if sev_snp_enabled() {
            // Make page invalid
            if let Err(_e) = pvalidate(vaddr, false, false) {
                return Err(());
            }

            // Let the Hypervisor take the page back
            if let Err(_e) = invalidate_page_msr(paddr) {
                return Err(());
            }

            // Needs guarding for Stage2 GHCB
            if valid_bitmap_valid_addr(paddr) {
                valid_bitmap_clear_valid_4k(paddr);
            }
        }

        // Map page unencrypted
        if let Err(_e) = get_init_pgtable_locked().set_shared_4k(vaddr) {
            return Err(());
        }

        flush_tlb_global_sync();

        Ok(())
    }

    pub fn register(&self) -> Result<(), ()> {
        let vaddr = (self as *const GHCB) as VirtAddr;
        let paddr = virt_to_phys(vaddr);

        // Register GHCB GPA
        register_ghcb_gpa_msr(paddr)
    }

    pub fn shutdown(&mut self) -> Result<(), ()> {
        let vaddr = (self as *const GHCB) as VirtAddr;
        let paddr = virt_to_phys(vaddr);

        // Re-encrypt page
        get_init_pgtable_locked().set_encrypted_4k(vaddr)?;

        // Unregister GHCB PA
        register_ghcb_gpa_msr(0usize)?;

        // Make page guest-invalid
        validate_page_msr(paddr)?;

        // Make page guest-valid
        if pvalidate(vaddr, false, true).is_err() {
            return Err(());
        }

        // Needs guarding for Stage2 GHCB
        if valid_bitmap_valid_addr(paddr) {
            valid_bitmap_set_valid_4k(paddr);
        }

        Ok(())
    }

    pub fn clear(&mut self) {
        // Clear valid bitmap
        self.valid_bitmap[0] = 0;
        self.valid_bitmap[1] = 0;

        // Mark valid_bitmap valid
        self.set_valid(OFF_VALID_BITMAP + 0);
        self.set_valid(OFF_VALID_BITMAP + 8);
    }

    fn set_valid(&mut self, offset: u16) {
        let bit: usize = (offset as usize >> 3) & 0x3f;
        let index: usize = (offset as usize >> 9) & 0x1;
        let mask: u64 = 1 << bit;

        self.valid_bitmap[index] |= mask;
    }

    fn is_valid(&self, offset: u16) -> bool {
        let bit: usize = (offset as usize >> 3) & 0x3f;
        let index: usize = (offset as usize >> 9) & 0x1;
        let mask: u64 = 1 << bit;

        (self.valid_bitmap[index] & mask) == mask
    }

    fn vmgexit(&mut self, exit_code: u64, exit_info_1: u64, exit_info_2: u64) -> Result<(), ()> {
        // GHCB is version 2
        self.version = 2;
        self.set_valid(OFF_VERSION);

        // GHCB Follows standard format
        self.usage = 0;
        self.set_valid(OFF_USAGE);

        self.sw_exit_code = exit_code;
        self.set_valid(OFF_SW_EXIT_CODE);

        self.sw_exit_info_1 = exit_info_1;
        self.set_valid(OFF_SW_EXIT_INFO_1);

        self.sw_exit_info_2 = exit_info_2;
        self.set_valid(OFF_SW_EXIT_INFO_2);

        unsafe {
            let ghcb_address = (self as *const GHCB) as VirtAddr;
            let ghcb_pa: u64 = virt_to_phys(ghcb_address) as u64;
            write_msr(SEV_GHCB, ghcb_pa);
            asm!("rep; vmmcall", options(att_syntax));
        }

        if self.is_valid(OFF_SW_EXIT_INFO_1) && self.sw_exit_info_1 == 0 {
            Ok(())
        } else {
            Err(())
        }
    }

    pub fn set_cpl(&mut self, cpl: u8) {
        self.cpl = cpl;
        self.set_valid(OFF_CPL);
    }

    pub fn set_dr7(&mut self, dr7: u64) {
        self.dr7 = dr7;
        self.set_valid(OFF_DR7);
    }

    pub fn set_xss(&mut self, xss: u64) {
        self.xss = xss;
        self.set_valid(OFF_XSS);
    }

    pub fn set_rax(&mut self, rax: u64) {
        self.rax = rax;
        self.set_valid(OFF_RAX);
    }

    pub fn set_rcx(&mut self, rcx: u64) {
        self.rcx = rcx;
        self.set_valid(OFF_RCX);
    }

    pub fn set_rdx(&mut self, rdx: u64) {
        self.rdx = rdx;
        self.set_valid(OFF_RDX);
    }

    pub fn set_rbx(&mut self, rbx: u64) {
        self.rbx = rbx;
        self.set_valid(OFF_RBX);
    }

    pub fn set_sw_scratch(&mut self, scratch: u64) {
        self.sw_scratch = scratch;
        self.set_valid(OFF_SW_SCRATCH);
    }

    pub fn set_sw_xcr0(&mut self, xcr0: u64) {
        self.xcr0 = xcr0;
        self.set_valid(OFF_XCR0);
    }

    pub fn set_sw_x87_state_gpa(&mut self, x87_state_gpa: u64) {
        self.x87_state_gpa = x87_state_gpa;
        self.set_valid(OFF_X87_STATE_GPA);
    }

    pub fn ioio_in(&mut self, port: u16, size: GHCBIOSize) -> Result<u64, ()> {
        self.clear();

        let mut info: u64 = 1; // IN instruction

        info |= (port as u64) << 16;

        match size {
            GHCBIOSize::Size8 => info |= 1 << 4,
            GHCBIOSize::Size16 => info |= 1 << 5,
            GHCBIOSize::Size32 => info |= 1 << 6,
        }

        match self.vmgexit(GHCBExitCode::IOIO, info, 0) {
            Ok(()) => {
                if self.is_valid(OFF_RAX) {
                    Ok(self.rax)
                } else {
                    Err(())
                }
            }
            Err(()) => Err(()),
        }
    }

    pub fn ioio_out(&mut self, port: u16, size: GHCBIOSize, value: u64) -> Result<(), ()> {
        self.clear();

        let mut info: u64 = 0; // OUT instruction

        info |= (port as u64) << 16;

        match size {
            GHCBIOSize::Size8 => info |= 1 << 4,
            GHCBIOSize::Size16 => info |= 1 << 5,
            GHCBIOSize::Size32 => info |= 1 << 6,
        }

        self.set_rax(value);

        self.vmgexit(GHCBExitCode::IOIO, info, 0)
    }

    fn write_buffer<T>(&mut self, data: &T, offset: isize) -> Result<(), ()>
    where T: Sized
    {
        let size: isize = mem::size_of::<T>() as isize;

        if offset < 0 || offset + size > (GHCB_BUFFER_SIZE as isize) {
            return Err(())
        }

        unsafe {
            let dst = self.buffer.as_mut_ptr().cast::<u8>().offset(offset).cast::<T>();
            let src = data as *const T;

            ptr::copy_nonoverlapping(src, dst, 1);
        }

        Ok(())
    }

    pub fn psc_entry(&self, paddr: PhysAddr, op_mask: u64, current_page: u64, huge: bool) -> u64 {
        assert!(huge == false || is_aligned(paddr, PAGE_SIZE_2M));

        let mut entry:u64 = ((paddr as u64) & PSC_GFN_MASK) | op_mask | (current_page & 0xfffu64);
        if huge {
            entry |= PSC_FLAG_HUGE;
        }

        entry
    }

    pub fn page_state_change(&mut self, start: PhysAddr, end: PhysAddr,
                             huge: bool, op: PageStateChangeOp) -> Result<(), ()> {
        // Maximum entries (8 bytes each_ minus 8 bytes for header
        let max_entries:u16 = ((GHCB_BUFFER_SIZE - 8) / 8).try_into().unwrap();
        let mut entries: u16 = 0;
        let mut vaddr = start;
        let op_mask: u64 = match op {
            PageStateChangeOp::PscPrivate => PSC_OP_PRIVATE,
            PageStateChangeOp::PscShared => PSC_OP_SHARED,
            PageStateChangeOp::PscPsmash => PSC_OP_PSMASH,
            PageStateChangeOp::PscUnsmash => PSC_OP_UNSMASH,
        };
        let pgsize: usize = match huge { true => PAGE_SIZE_2M, false => PAGE_SIZE }; 

        self.clear();

        while vaddr < end {
            let entry = self.psc_entry(vaddr, op_mask, 0, huge);
            let offset: isize = (entries as isize) * 8 + 8;
            self.write_buffer(&entry, offset)?;
            entries += 1;
            vaddr += pgsize;

            if entries == max_entries {
                let header = PageStateChangeHeader { cur_entry: 0, end_entry: entries - 1, reserved: 0 };
                self.write_buffer(&header, 0)?;
                
                let buffer_va = self.buffer.as_ptr() as VirtAddr;
                let buffer_pa: u64 = virt_to_phys(buffer_va) as u64;
                self.set_sw_scratch(buffer_pa);

                if let Err(_) = self.vmgexit(GHCBExitCode::SNP_PSC, 0, 0) {
                    if !self.is_valid(OFF_SW_EXIT_INFO_2) {
                        return Err(());
                    }

                    let info_high: u32 = (self.sw_exit_info_2 >> 32) as u32;
                    let info_low: u32 = (self.sw_exit_info_2 & 0xffff_ffffu64) as u32;

                    log::error!("GHCB SnpPageStateChange failed err_high: {:#x} err_low: {:#x}", info_high, info_low);

                    return Err(());
                }

                entries = 0;
            }
        }

        Ok(())
    }

    pub fn ap_create(
        &mut self,
        vmsa_gpa: PhysAddr,
        apic_id: u64,
        vmpl: u64,
        sev_features: u64,
    ) -> Result<(), ()> {
        self.clear();
        let exit_info_1: u64 = 1 | (vmpl & 0xf) << 16 | apic_id << 32;
        let exit_info_2: u64 = vmsa_gpa as u64;
        self.set_rax(sev_features);
        self.vmgexit(GHCBExitCode::AP_CREATE, exit_info_1, exit_info_2)
    }

    pub fn run_vmpl(&mut self, vmpl: u64) -> Result<(),()> {
        self.clear();
        self.vmgexit(GHCBExitCode::RUN_VMPL, vmpl, 0)
    }
}

pub struct GHCBIOPort<'a> {
    pub ghcb: RefCell<&'a mut GHCB>,
}

impl<'a> GHCBIOPort<'a> {
    pub fn new(ghcb: RefCell<&'a mut GHCB>) -> Self {
        GHCBIOPort { ghcb: ghcb }
    }
}
unsafe impl<'a> Sync for GHCBIOPort<'a> {}

impl<'a> IOPort for GHCBIOPort<'a> {
    fn outb(&self, port: u16, value: u8) {
        let mut g = self.ghcb.borrow_mut();
        let ret = g.ioio_out(port, GHCBIOSize::Size8, value as u64);
        if let Err(()) = ret {
            request_termination_msr();
        }
    }

    fn inb(&self, port: u16) -> u8 {
        let mut g = self.ghcb.borrow_mut();
        let ret = g.ioio_in(port, GHCBIOSize::Size8);
        match ret {
            Ok(v) => (v & 0xff) as u8,
            Err(_e) => {
                request_termination_msr();
                0
            }
        }
    }

    fn outw(&self, port: u16, value: u16) {
        let mut g = self.ghcb.borrow_mut();
        let ret = g.ioio_out(port, GHCBIOSize::Size16, value as u64);
        if let Err(()) = ret {
            request_termination_msr();
        }
    }

    fn inw(&self, port: u16) -> u16 {
        let mut g = self.ghcb.borrow_mut();
        let ret = g.ioio_in(port, GHCBIOSize::Size16);
        match ret {
            Ok(v) => (v & 0xffff) as u16,
            Err(_e) => {
                request_termination_msr();
                0
            }
        }
    }
}
