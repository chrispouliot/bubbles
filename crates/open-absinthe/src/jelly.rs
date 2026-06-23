//! Mach-O loader + Unicorn x86-64 harness — the Rust port of the sidecar's
//! `jelly.py`. It maps the (already x86-64-sliced) `IMDAppleServices` binary
//! into an emulator, wires its imported symbols to in-process hooks
//! (`hooks.rs`), and provides a SysV calling convention so `nac.rs` can call
//! `nacInit`/`nacKeyEstablishment`/`nacSign` at their fixed offsets.
//!
//! Verified against the real binary (sha1 e1181cc…): goblin parses the fat
//! container, yields the x86-64 slice, and `imports()` resolves every dyld bind
//! to `(name, address)` — which replaces jelly.py's hand-rolled bind-opcode
//! parser entirely. Each hooked import's pointer slot is overwritten with an
//! address in a `ret`-filled hook page; a single code hook over that page
//! dispatches to `hooks::dispatch`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use goblin::mach::cputype::CPU_TYPE_X86_64;
use goblin::mach::{Mach, MachO};
use unicorn_engine::unicorn_const::{Arch, Mode, Prot};
use unicorn_engine::{RegisterX86, Unicorn};

use crate::AbsintheError;
use crate::hooks::{self, NacState};
use crate::nac::HardwareConfig;

// Fixed memory map — identical to jelly.py so the hard-coded function offsets
// (which assume the binary is based at 0) stay valid.
const BINARY_BASE: u64 = 0x0;
const HOOK_BASE: u64 = 0xD0_0000;
const HOOK_SIZE: u64 = 0x1000;
const STACK_BASE: u64 = 0x30_0000;
const STACK_SIZE: u64 = 0x10_0000;
const HEAP_BASE: u64 = 0x40_0000;
const HEAP_SIZE: u64 = 0x10_0000;
const STOP_ADDRESS: u64 = 0x90_0000;
const PAGE: u64 = 0x1000;

// SysV AMD64 integer argument registers, in order.
const ARG_REGS: [RegisterX86; 6] = [
    RegisterX86::RDI,
    RegisterX86::RSI,
    RegisterX86::RDX,
    RegisterX86::RCX,
    RegisterX86::R8,
    RegisterX86::R9,
];

pub(crate) struct Jelly {
    pub uc: Unicorn<'static, ()>,
    /// Shared emulator-side state (heap pointer, CF object table, IOKit values).
    /// Held both here and (cloned) inside the code-hook closure.
    pub state: Rc<RefCell<NacState>>,
}

impl Jelly {
    /// Build an emulator over `slice` (the x86-64 Mach-O), seeded with `hw`.
    pub fn new(slice: &[u8], hw: &HardwareConfig) -> Result<Jelly, AbsintheError> {
        let mut state = NacState::from_hw(hw);

        // Assign each hooked symbol a unique 1-byte address in the hook page,
        // and remember the reverse map for dispatch.
        let mut resolved: HashMap<&'static str, u64> = HashMap::new();
        for (i, name) in hooks::HOOKS.iter().enumerate() {
            let addr = HOOK_BASE + i as u64;
            resolved.insert(name, addr);
            state.hook_addr_to_name.insert(addr, name);
        }
        let state = Rc::new(RefCell::new(state));

        let mut uc = Unicorn::new(Arch::X86, Mode::MODE_64)?;

        // Hook page: filled with `ret` (0xC3). Execution that lands on a hook
        // address triggers the code hook below, then returns to the caller.
        uc.mem_map(HOOK_BASE, HOOK_SIZE, Prot::ALL)?;
        uc.mem_write(HOOK_BASE, &vec![0xC3u8; HOOK_SIZE as usize])?;

        let hook_state = state.clone();
        uc.add_code_hook(HOOK_BASE, HOOK_BASE + HOOK_SIZE, move |uc, addr, _size| {
            let mut st = hook_state.borrow_mut();
            if let Some(name) = st.hook_addr_to_name.get(&addr).copied() {
                hooks::dispatch(uc, &mut st, name);
            }
        })?;

        // Map the binary 1:1 at base 0 (vmaddr == fileoff for this binary).
        let mapped = round_to_page(slice.len() as u64);
        uc.mem_map(BINARY_BASE, mapped, Prot::ALL)?;
        uc.mem_write(BINARY_BASE, slice)?;
        // Drop page 0 so NULL derefs fault instead of reading the Mach-O header.
        uc.mem_unmap(0, PAGE)?;

        // Resolve imports: goblin decodes every bind to (name, address); for the
        // symbols we hook, overwrite the pointer slot with the hook address.
        let macho = MachO::parse(slice, 0)?;
        let mut bound = 0usize;
        for imp in macho.imports()? {
            if let Some(&hook_addr) = resolved.get(imp.name) {
                uc.mem_write(imp.address, &hook_addr.to_le_bytes())?;
                bound += 1;
            }
        }
        log::debug!("bound {bound}/{} hook symbols", hooks::HOOKS.len());

        // Stack.
        uc.mem_map(STACK_BASE, STACK_SIZE, Prot::ALL)?;
        let sp = STACK_BASE + STACK_SIZE;
        uc.reg_write(RegisterX86::RSP, sp)?;
        uc.reg_write(RegisterX86::RBP, sp)?;

        // Heap (bump-allocated by `NacState::malloc`).
        uc.mem_map(HEAP_BASE, HEAP_SIZE, Prot::ALL)?;

        // Stop page: emu_start returns here (it is the fake return address).
        uc.mem_map(STOP_ADDRESS, PAGE, Prot::ALL)?;
        uc.mem_write(STOP_ADDRESS, &vec![0xC3u8; PAGE as usize])?;

        Ok(Jelly { uc, state })
    }

    /// Bump-allocate `len` bytes on the emulated heap, return the address.
    pub fn malloc(&self, len: usize) -> u64 {
        let mut st = self.state.borrow_mut();
        let addr = HEAP_BASE + st.heap_use;
        st.heap_use += len as u64;
        addr
    }

    fn push(&mut self, value: u64) -> Result<(), AbsintheError> {
        let sp = self.uc.reg_read(RegisterX86::RSP)? - 8;
        self.uc.reg_write(RegisterX86::RSP, sp)?;
        self.uc.mem_write(sp, &value.to_le_bytes())?;
        Ok(())
    }

    /// Call `address` with up to 6 integer args (extra args unsupported — none
    /// of the nac entrypoints need them). Returns RAX.
    pub fn call(&mut self, address: u64, args: &[u64]) -> Result<u64, AbsintheError> {
        log::debug!("call {address:#x} args={args:x?}");
        // Fake return address so the function `ret`s into the stop page.
        self.push(STOP_ADDRESS)?;
        for (i, &a) in args.iter().enumerate() {
            if i < 6 {
                self.uc.reg_write(ARG_REGS[i], a)?;
            } else {
                return Err(AbsintheError::new(-3));
            }
        }
        self.uc.emu_start(address, STOP_ADDRESS, 0, 0)?;
        Ok(self.uc.reg_read(RegisterX86::RAX)?)
    }

    pub fn read(&self, addr: u64, len: usize) -> Result<Vec<u8>, AbsintheError> {
        Ok(self.uc.mem_read_as_vec(addr, len)?)
    }

    pub fn write(&mut self, addr: u64, data: &[u8]) -> Result<(), AbsintheError> {
        self.uc.mem_write(addr, data)?;
        Ok(())
    }

    pub fn read_u64(&self, addr: u64) -> Result<u64, AbsintheError> {
        let b = self.read(addr, 8)?;
        Ok(u64::from_le_bytes(b.try_into().unwrap()))
    }

    /// Pull the x86-64 slice out of the fat `IMDAppleServices` container.
    pub fn extract_x86_64(full: &[u8]) -> Result<Vec<u8>, AbsintheError> {
        match Mach::parse(full)? {
            Mach::Fat(fat) => {
                for arch in fat.arches()? {
                    if arch.cputype == CPU_TYPE_X86_64 {
                        let off = arch.offset as usize;
                        let size = arch.size as usize;
                        return Ok(full[off..off + size].to_vec());
                    }
                }
                Err(AbsintheError::new(-4)) // no x86-64 slice
            }
            Mach::Binary(_) => Ok(full.to_vec()), // already thin
        }
    }
}

fn round_to_page(size: u64) -> u64 {
    (size + PAGE - 1) & !(PAGE - 1)
}
