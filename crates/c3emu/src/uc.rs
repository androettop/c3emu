//! CPU backend: Unicorn (native builds, JIT) or the built-in ARM926 interpreter
//! (src/cpu: WebAssembly, or native builds without the `unicorn` feature). Both offer
//! the same API subset; the rest of the emulator only names the types re-exported here.

#[cfg(feature = "unicorn")]
pub use unicorn_engine::{uc_error, HookType, MemType, Prot, RegisterARM, RegisterARMCP, UcHookId};

#[cfg(feature = "unicorn")]
pub type Uc<'a> = unicorn_engine::Unicorn<'a, ()>;

#[cfg(feature = "unicorn")]
pub fn new_cpu<'a>() -> Result<Uc<'a>, uc_error> {
    use unicorn_engine::{Arch, ArmCpuModel, Mode, Unicorn};
    let mut uc = Unicorn::new(Arch::ARM, Mode::ARM)?;
    uc.ctl_set_cpu_model(ArmCpuModel::Model_926 as i32)?; // before mem_map
    Ok(uc)
}

#[cfg(not(feature = "unicorn"))]
pub use crate::cpu::{uc_error, HookType, MemType, Prot, RegisterARM, RegisterARMCP, UcHookId};

#[cfg(not(feature = "unicorn"))]
pub type Uc<'a> = crate::cpu::Cpu<'a>;

#[cfg(not(feature = "unicorn"))]
pub fn new_cpu<'a>() -> Result<Uc<'a>, uc_error> {
    Ok(crate::cpu::Cpu::new())
}
