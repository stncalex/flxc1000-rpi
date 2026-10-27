use std::{
    fmt::Debug,
    sync::atomic::Ordering,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use embedded_graphics::{
    mono_font::{ascii::FONT_6X10, MonoTextStyleBuilder},
    pixelcolor::BinaryColor,
    prelude::*,
    text::{Baseline, Text},
};

use rppal::i2c::I2c;
use ssd1306::{prelude::*, I2CDisplayInterface, Ssd1306};

use super::{SharedState, FLUX_ERROR, FLUX_READING, FLUX_READY};

const OLED_ADDRESS: u8 = 0x3C;

// ====================================================================================================================================================================================
// DIAGNOSTIC DISPLAY LOGIC
// ====================================================================================================================================================================================

// Duration for which the information stays on the screen
const DIAGNOSTIC_DISPLAY_DURATION: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct DiagnosticScreen {
    pub title: String,
    pub value: String,
    expires_at: Instant,
}

impl DiagnosticScreen {
    // Creates a new diagnostic screen with a 10-second timeout

    pub fn new(title: &str, value: &str) -> Self {
        Self {
            title: title.to_string(),
            value: value.to_string(),
            expires_at: Instant::now() + DIAGNOSTIC_DISPLAY_DURATION,
        }
    }

    // Checks whether the diagnostic screen should still be visible.
    pub fn is_active(&self) -> bool {
        Instant::now() < self.expires_at
    }
}
// ========================================================================================================================
// NORMAL OLED FUNCTIONALITY
// ========================================================================================================================

pub fn spawn_status_task(shared: Arc<SharedState>) {
    thread::spawn(move || {
        if let Err(e) = run(shared) {
            tracing::warn!("OLED disabled: {}", e);
        }
    });
}

fn run(shared: Arc<SharedState>) -> Result<(), String> {
    match std::fs::read("/proc/device-tree/soc/i2c@7e804000/status") {
        Ok(bytes) => {
            let status = String::from_utf8_lossy(&bytes);
            tracing::info!("Runtime DT I2C1 status: {}", status.trim_end_matches('\0'));
        }
        Err(e) => {
            tracing::warn!("Could not read runtime DT I2C1 status: {}", e);
        }
    }

    for path in ["/dev/i2c-0", "/dev/i2c-1", "/dev/i2c-3"] {
        tracing::info!("{} exists: {}", path, std::path::Path::new(path).exists());
    }
    let mut i2c = None;
    let mut last_error = String::new();

    for attempt in 1..=50 {
        match I2c::new() {
            Ok(bus) => {
                tracing::info!("I2C available after attempt {}", attempt);
                i2c = Some(bus);
                break;
            }

            Err(e) => {
                last_error = e.to_string();

                if attempt == 1 || attempt % 10 == 0 {
                    tracing::warn!("Waiting for I2C, attempt {}/50: {}", attempt, e);
                }

                thread::sleep(Duration::from_millis(200));
            }
        }
    }

    let i2c = i2c.ok_or_else(|| format!("Failed to open I2C after 10 seconds: {}", last_error))?;

    let interface = I2CDisplayInterface::new_custom_address(i2c, OLED_ADDRESS);

    let mut display = Ssd1306::new(interface, DisplaySize128x64, DisplayRotation::Rotate0)
        .into_buffered_graphics_mode();

    display.init().map_err(fmt_err("init oled"))?;

    tracing::info!("OLED initialized successfully");

    loop {
        // Check whether a temporary diagnostic screen is active
        let diagnostic = {
            let mut screen = shared
                .diagnostic_screen
                .lock()
                .map_err(fmt_err("lock diagnostic screen"))?;

            // Remove the message if its 10-second timeout has expired.
            if screen.as_ref().is_some_and(|s| !s.is_active()) {
                *screen = None;
            }

            // Clone the message so we can release the lock before drawing
            screen.clone()
        };

        // Select which screen to display
        if let Some(screen) = diagnostic {
            draw_diagnostic(&mut display, &screen)?;
        } else {
            draw_status(&mut display, &shared)?;
        }

        // Send the updated image to the OLED.
        display.flush().map_err(fmt_err("flush oled"))?;

        thread::sleep(Duration::from_millis(500));
    }
}

fn draw_status<D>(display: &mut D, shared: &SharedState) -> Result<(), String>
where
    D: DrawTarget<Color = BinaryColor>,
    D::Error: Debug,
{
    display
        .clear(BinaryColor::Off)
        .map_err(fmt_err("clear oled"))?;

    let style = MonoTextStyleBuilder::new()
        .font(&FONT_6X10)
        .text_color(BinaryColor::On)
        .build();

    let version = option_env!("FLXC1000_BUILD_VERSION").unwrap_or("unknown");
    let flux = flux_name(shared.flux_state.load(Ordering::Relaxed));
    let power = shared.flux_capacitor_power;

    let err = shared
        .last_error
        .lock()
        .ok()
        .and_then(|e| e.clone())
        .unwrap_or_else(|| "none".to_string());

    let eth = ethernet_status();

    let lines = [
        format!("FLXC v{}", short(version, 14)),
        format!("Flux {}", flux),
        format!("Power {}", power),
        format!("Err {}", short(&err, 17)),
        format!("Eth {}", eth),
    ];

    for (i, line) in lines.iter().enumerate() {
        Text::with_baseline(line, Point::new(0, i as i32 * 12), style, Baseline::Top)
            .draw(display)
            .map_err(fmt_err("draw oled"))?;
    }

    Ok(())
}

fn draw_diagnostic<D>(display: &mut D, screen: &DiagnosticScreen) -> Result<(), String>
where
    D: DrawTarget<Color = BinaryColor>,
    D::Error: Debug,
{
    // Clear previous screen contents
    display
        .clear(BinaryColor::Off)
        .map_err(fmt_err("clear diagnostic oled"))?;

    let style = MonoTextStyleBuilder::new()
        .font(&FONT_6X10)
        .text_color(BinaryColor::On)
        .build();

    // Header
    Text::with_baseline("DIAGNOSTIC INFO", Point::new(0, 0), style, Baseline::Top)
        .draw(display)
        .map_err(fmt_err("draw diagnostic header"))?;

    // Requested diagnostic parameter
    Text::with_baseline(
        &short(&screen.title, 21),
        Point::new(0, 22),
        style,
        Baseline::Top,
    )
    .draw(display)
    .map_err(fmt_err("draw diagnostic title"))?;

    // Diagnostic response value
    Text::with_baseline(
        &short(&screen.value, 21),
        Point::new(0, 42),
        style,
        Baseline::Top,
    )
    .draw(display)
    .map_err(fmt_err("draw diagnostic value"))?;

    Ok(())
}

fn flux_name(value: u8) -> &'static str {
    match value {
        FLUX_READY => "READY",
        FLUX_READING => "READING",
        FLUX_ERROR => "ERROR",
        _ => "UNKNOWN",
    }
}

fn ethernet_status() -> &'static str {
    let interfaces = match std::fs::read_dir("/sys/class/net") {
        Ok(entries) => entries,
        Err(_) => return "disconnected",
    };

    for entry in interfaces.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();

        // Ethernet interfaces are normally called eth0, enp..., eno..., etc.
        if name.starts_with("eth") || name.starts_with("en") {
            let carrier_path = entry.path().join("carrier");

            if let Ok(value) = std::fs::read_to_string(carrier_path) {
                if value.trim() == "1" {
                    return "connected";
                }
            }
        }
    }

    "disconnected"
}

fn short(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

fn fmt_err<E: Debug>(context: &'static str) -> impl FnOnce(E) -> String {
    move |e| format!("{context}: {e:?}")
}

// ============================================================
// UNIT TESTS
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_diagnostic_screen_is_active() {
        let screen = DiagnosticScreen::new("SOFTWARE VERSION", "V2");

        assert!(screen.is_active());
    }

    #[test]
    fn expired_diagnostic_screen_is_inactive() {
        let mut screen = DiagnosticScreen::new("SOFTWARE VERSION", "V2");

        // Simulate a screen that expired one sec ago
        screen.expires_at = Instant::now() - Duration::from_secs(1);

        assert!(!screen.is_active());
    }

    #[test]
    fn diagnostic_screen_can_be_replaced() {
        let mut screen = DiagnosticScreen::new("SOFTWARE VERSION", "V1");

        // Verify the original diagnostic message
        assert_eq!(screen.title, "SOFTWARE VERSION");
        assert_eq!(screen.value, "V1");

        // Simulate receiving another diagnostic request
        screen = DiagnosticScreen::new("ACTIVE SLOT", "V2");

        assert_eq!(screen.title, "ACTIVE SLOT");
        assert_eq!(screen.value, "V2");
        assert!(screen.is_active());
    }
}
