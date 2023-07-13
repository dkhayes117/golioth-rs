// use core::sync::atomic::{AtomicUsize, Ordering};
// use embassy_time::Instant;

// use core::sync::atomic::AtomicUsize;

use alloc::format;
use alloc::string::String;
use at_commands::parser::CommandParser;
use defmt::{Debug2Format,trace};
use nanorand::{Rng, WyRand};
use crate::errors::Error;
use crate::LightDBType;

// same panicking *behavior* as `panic-probe` but doesn't print a panic message
// this prevents the panic message being printed *twice* when `defmt::panic` is invoked
#[defmt::panic_handler]
fn panic() -> ! {
    cortex_m::asm::udf()
}

// static COUNT: AtomicUsize = AtomicUsize::new(0);
// defmt::timestamp!("{=usize}", {
//     // NOTE(no-CAS) `timestamps` runs with interrupts disabled
//     let n = COUNT.load(Ordering::Relaxed);
//     COUNT.store(n + 1, Ordering::Relaxed);
//     n
// });

/// Terminates the application and makes `probe-run` exit with exit-code = 0
pub fn exit() -> ! {
    loop {
        cortex_m::asm::bkpt();
    }
}

#[inline]
pub fn get_formatted_path(db_type: LightDBType, path: &str) -> String {
    match db_type {
        LightDBType::State => {
            format!(".d/{}", path)
        }
        LightDBType::Stream => {
            format!(".s/{}", path)
        }
    }
}

/// The nRF9160 does not have a RNG peripheral.  To bypass using the Cryptocell C-lib
/// we can just use the current uptime ticks u64 value as a way to create a unique "enough"
/// token as to not be easily spoofed.
pub fn create_token() -> [u8; 8] {
    let seed = embassy_time::Instant::now().as_ticks();
    let mut rng = WyRand::new_seed(seed);
    let token: [u8; 8] = rng.generate::<u64>().to_ne_bytes();

    trace!("WyRand Request Token: {}", Debug2Format(&token));
    token
}

/// Get the current signal strength from the modem using AT+CESQ
pub async fn get_signal_strength() -> Result<i32, Error> {
    let command = nrf_modem::send_at::<32>("AT+CESQ").await?;

    let (_, _, _, _, _, mut signal) = CommandParser::parse(command.as_bytes())
        .expect_identifier(b"+CESQ:")
        .expect_int_parameter()
        .expect_int_parameter()
        .expect_int_parameter()
        .expect_int_parameter()
        .expect_int_parameter()
        .expect_int_parameter()
        .expect_identifier(b"\r\n")
        .finish()
        .unwrap();
    if signal != 255 {
        signal += -140;
    }
    Ok(signal)
}
