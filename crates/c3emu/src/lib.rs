//! c3emu: emulator for the Nokia C3-00 (RM-614). ARM926EJ-S core (built-in
//! interpreter, or optionally Unicorn), the RM-614 SoC devices the Series 40 OS needs,
//! and high-level replacements (HLE) for what cannot exist without the real hardware.

pub mod analog;
pub mod clock;
#[cfg(not(feature = "unicorn"))]
pub mod cpu;
pub mod uc;
pub mod audio;
pub mod bb5;
pub mod boot;
pub mod bootinfo;
pub mod bus;
pub mod content;
pub mod dmac;
pub mod dsp;
pub mod fpsx;
pub mod fxhash;
pub mod gpio;
pub mod gptimer;
pub mod i2c;
pub mod isi;
pub mod keypad;
pub mod layout;
pub mod lcd;
pub mod machine;
pub mod mmu;
pub mod nucleus;
pub mod oslog;
pub mod patches;
pub mod rtc;
pub mod seccall;
pub mod sim;
pub mod skin;
pub mod soc;
#[cfg(feature = "desktop")]
pub mod sound;
pub mod storage;
