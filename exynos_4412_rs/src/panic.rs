//! Default panic handler, enabled by the `panic-handler` feature.

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::cpu;
use crate::println;

// Plain load and store, see the `console` module documentation.
static PANICKING: AtomicBool = AtomicBool::new(false);

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    if !PANICKING.load(Ordering::Relaxed) {
        PANICKING.store(true, Ordering::Relaxed);
        match info.location() {
            Some(location) => {
                println!("\n*** CPU{} panic at {}:{}", cpu::id(), location.file(), location.line());
            }
            None => println!("\n*** CPU{} panic", cpu::id()),
        }
        println!("{}", info.message());
    }
    // A buffered console would keep the report in its ring: see `console`.
    crate::console::flush();
    cpu::halt();
}
