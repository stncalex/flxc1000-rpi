use rs_ws281x::{ChannelBuilder, ControllerBuilder, StripType};
// Imports 3 things from the library we use
// ControllerBuilder   - creates the overall LED controller
// ChannelBuilder      - configures one LED output channel, ex: GPIO18
// StripType           - tells the library what kind of LED strip we use, ws2812

use std::{
    sync::{
        atomic::Ordering, // safely read atomic shared variables
        Arc,              // share the same data between different parts of the program
    },
    thread,         // run the RGB logic in the background
    time::Duration, // represents time, like 50 ms
};

use super::SharedState;

const COMMUNICATION_DELAY_MS: u64 = 45;
const MAIN_LED_COUNT: i32 = 24;
const SELF_TEST_LED_COUNT: i32 = 8;

const BRANCH_COUNT: usize = 3;
const BRANCH_LENGTH: usize = 8;
const ANIMATION_DELAY_MS: u64 = 90;
const RGB_REFRESH_MS: u64 = 10;
const LED_PULSE_DURATION_MS: u64 = 150;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LedVariant {
    Blue,
    Green,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LedMode {
    Waiting,
    Blue,
    Green,
    Idle,
}

struct IdleColors {
    background: [u8; 4],
    pulse: [u8; 4],
    trail_1: [u8; 4],
    trail_2: [u8; 4],
    center: [u8; 4],
}

impl LedVariant {
    fn colors(self) -> IdleColors {
        match self {
            LedVariant::Blue => IdleColors {
                background: [5, 15, 30, 0],
                pulse: [120, 220, 255, 0],
                trail_1: [50, 120, 200, 0],
                trail_2: [15, 45, 90, 0],
                center: [180, 255, 255, 0],
            },

            LedVariant::Green => IdleColors {
                background: [5, 30, 5, 0],
                pulse: [80, 255, 100, 0],
                trail_1: [30, 180, 50, 0],
                trail_2: [10, 80, 20, 0],
                center: [150, 255, 150, 0],
            },
        }
    }
}

const VARIANT_CONFIG_PATH: &str = "/data/variant.conf";
const VARIANT_CHECK_INTERVAL: Duration = Duration::from_secs(1);

fn parse_led_mode(value: &str) -> LedMode {
    match value.trim().to_ascii_lowercase().as_str() {
        "blue" => LedMode::Blue,
        "green" => LedMode::Green,
        "idle" => LedMode::Idle,
        "waiting" => LedMode::Waiting,
        _ => LedMode::Waiting,
    }
}

fn load_led_mode() -> LedMode {
    match std::fs::read_to_string(VARIANT_CONFIG_PATH) {
        Ok(value) => parse_led_mode(&value),
        Err(_) => LedMode::Waiting,
    }
}

// Brightness levels corresponding to the old 8 LEDs
const LEVELS: [u8; 8] = [80, 105, 130, 155, 180, 205, 230, 255];

pub fn spawn_status_task(shared: Arc<SharedState>) {
    thread::spawn(move || {
        if let Err(e) = run(shared) {
            tracing::warn!("RGB status disabled: {}", e);
        }
    });
}
fn run(shared: Arc<SharedState>) -> Result<(), String> {
    let mut controller = ControllerBuilder::new()
        .freq(800_000)
        .dma(10)
        // RGB #1 - main Flux Capacitor LEDs
        // GPIO18 = physical pin 12
        .channel(
            0,
            ChannelBuilder::new()
                .pin(18)
                .count(MAIN_LED_COUNT)
                .strip_type(StripType::Ws2812)
                .brightness(32)
                .build(),
        )
        // RGB #2 - reserved for a future hardware expansion.
        // GPIO13 = physical pin 33 (currently no LEDs connected)
        .channel(
            1,
            ChannelBuilder::new()
                .pin(13)
                .count(SELF_TEST_LED_COUNT)
                .strip_type(StripType::Ws2812)
                .brightness(32)
                .build(),
        )
        .build()
        .map_err(|e| format!("Failed to initialize RGB LEDs: {e:?}"))?;

    render_boot_blink(&mut controller)?;

    let mut animation_position: usize = 0;
    let mut next_animation_update = std::time::Instant::now();
    let mut last_mask: Option<u32> = None;
    let mut last_connected: Option<bool> = None;
    let mut last_diagnostic_active: Option<bool> = None;
    let mut last_self_test_active: Option<bool> = None;

    // Track requests to flash LED 1
    let mut last_pulse_request = shared.led_pulse_requests.load(Ordering::Relaxed);
    let mut pulse_until: Option<std::time::Instant> = None;
    let mut mode = load_led_mode();
    let mut last_variant_check = std::time::Instant::now();
    let mut variant_changed = false;

    tracing::info!("Selected LED variant: {:?}", mode);

    loop {
        if last_variant_check.elapsed() >= VARIANT_CHECK_INTERVAL {
            let new_mode = load_led_mode();

            if new_mode != mode {
                tracing::info!("LED variant changed: {:?} -> {:?}", mode, new_mode);

                mode = new_mode;
                animation_position = 0;
                variant_changed = true;
            }

            last_variant_check = std::time::Instant::now();
        }

        let connected = ethernet_connected();

        // Notify the OLED only when Ethernet connectivity changes.
        if last_connected != Some(connected) {
            let sender = shared.oled_tx.lock().ok().and_then(|guard| guard.clone());

            if let Some(sender) = sender {
                match sender.try_send(crate::oled::OledEvent::EthernetChanged(connected)) {
                    Ok(()) => {}

                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        // A queued event will trigger a refresh from the latest state.
                    }

                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                        tracing::debug!("OLED event channel is closed");
                    }
                }
            }
        }

        let diagnostic_active = shared.diagnostic_activity.load(Ordering::Relaxed);
        let mut mask = shared.rgb_led_mask.load(Ordering::SeqCst);
        let now = std::time::Instant::now();
        let self_test_active =
            shared.routine_status.load(Ordering::SeqCst) == super::ROUTINE_RUNNING;

        // Check whether a new LED pulse was requested.
        let pulse_request = shared.led_pulse_requests.load(Ordering::Relaxed);

        let mut pulse_state_changed = false;

        if pulse_request != last_pulse_request {
            last_pulse_request = pulse_request;
            pulse_until = Some(now + Duration::from_millis(LED_PULSE_DURATION_MS));
            pulse_state_changed = true;
        }

        // Keep LED 1 illuminated while the pulse is active.
        if let Some(deadline) = pulse_until {
            if now < deadline {
                mask |= 1u32;
            } else {
                pulse_until = None;
                pulse_state_changed = true;
            }
        }

        // Update the eight-LED bar when its mask changes.
        let mask_changed = last_mask != Some(mask);

        if mask_changed {
            render_self_test_bar(&mut controller, mask);
            last_mask = Some(mask);
        }

        // Update the main animation only when needed.
        let state_changed = last_connected != Some(connected)
            || last_diagnostic_active != Some(diagnostic_active)
            || last_self_test_active != Some(self_test_active);

        let animation_due = now >= next_animation_update;

        let mut frame_updated = false;

        if state_changed
            || mask_changed
            || variant_changed
            || pulse_state_changed
            || (!self_test_active && animation_due)
        {
            if self_test_active {
                // SelfTest takes priority over normal animations.
                render_self_test_main(&mut controller, mask);
            } else if !connected {
                // Ethernet disconnected: solid red.
                for led in controller.leds_mut(0).iter_mut() {
                    *led = [0, 0, 255, 0];
                }

                next_animation_update = now + Duration::from_millis(ANIMATION_DELAY_MS);
            } else if diagnostic_active {
                render_flux_communication(&mut controller, animation_position)?;

                animation_position += 1;

                if animation_position >= BRANCH_LENGTH {
                    animation_position = 0;
                    shared.diagnostic_activity.store(false, Ordering::Relaxed);
                }

                next_animation_update = now + Duration::from_millis(COMMUNICATION_DELAY_MS);
            } else {
                match mode {
                    LedMode::Waiting => {
                        render_flux_idle(&mut controller, animation_position, LedVariant::Blue)?;

                        let delay = if animation_position == BRANCH_LENGTH - 1 {
                            ANIMATION_DELAY_MS + 200
                        } else {
                            ANIMATION_DELAY_MS
                        };

                        animation_position = (animation_position + 1) % BRANCH_LENGTH;
                        next_animation_update = now + Duration::from_millis(delay);
                    }

                    LedMode::Blue => {
                        // for led in controller.leds_mut(0).iter_mut() {
                        //     *led = [255, 0, 0, 0];
                        // }

                        // next_animation_update = now + Duration::from_millis(ANIMATION_DELAY_MS);

                        let leds = controller.leds_mut(0);

                        for (index, led) in leds.iter_mut().enumerate() {
                            *led = match index {
                                0..=7 => [0, 0, 255, 0],   // Red
                                8..=15 => [0, 255, 0, 0],  // Green
                                16..=23 => [255, 0, 0, 0], // Blue
                                _ => [0, 0, 0, 0],
                            };
                        }

                        next_animation_update = now + Duration::from_millis(ANIMATION_DELAY_MS);
                    }

                    LedMode::Green => {
                        for led in controller.leds_mut(0).iter_mut() {
                            *led = [0, 255, 0, 0];
                        }

                        next_animation_update = now + Duration::from_millis(ANIMATION_DELAY_MS);
                    }

                    LedMode::Idle => {
                        render_flux_idle(&mut controller, animation_position, LedVariant::Blue)?;

                        let delay = if animation_position == BRANCH_LENGTH - 1 {
                            ANIMATION_DELAY_MS + 200
                        } else {
                            ANIMATION_DELAY_MS
                        };

                        animation_position = (animation_position + 1) % BRANCH_LENGTH;

                        next_animation_update = now + Duration::from_millis(delay);
                    }
                }
            }

            // Show the diagnostic LED pulse on the physical RGB channel.
            // SelfTest and Ethernet-disconnected status retain priority.
            if connected && !self_test_active && pulse_until.is_some() {
                controller.leds_mut(0)[0] = [0, 255, 0, 0];
            }

            last_connected = Some(connected);
            last_diagnostic_active = Some(diagnostic_active);
            last_self_test_active = Some(self_test_active);
            variant_changed = false;
            frame_updated = true;
        }

        // Send both channels to the physical LEDs.
        if mask_changed || frame_updated {
            controller
                .render()
                .map_err(|e| format!("Failed to render RGB LEDs: {e:?}"))?;
        }

        thread::sleep(Duration::from_millis(RGB_REFRESH_MS));
    }
}

fn render_flux_idle(
    controller: &mut rs_ws281x::Controller,
    position: usize,
    variant: LedVariant,
) -> Result<(), String> {
    let colors = variant.colors();
    let leds = controller.leds_mut(0);

    // Soft background glow
    for led in leds.iter_mut() {
        *led = colors.background;
    }

    // Pulse on all three branches
    for branch in 0..BRANCH_COUNT {
        let base = branch * BRANCH_LENGTH;

        // Reverse the red stick (branch 0).
        // Preserve the existing reversal of branch 2.
        let reversed = branch == 0 || branch == 2;

        let current = if reversed {
            base + (BRANCH_LENGTH - 1 - position)
        } else {
            base + position
        };

        leds[current] = colors.pulse;

        if position >= 1 {
            let trail_1 = if reversed {
                base + (BRANCH_LENGTH - position)
            } else {
                base + position - 1
            };

            leds[trail_1] = colors.trail_1;
        }

        if position >= 2 {
            let trail_2 = if reversed {
                base + (BRANCH_LENGTH + 1 - position)
            } else {
                base + position - 2
            };

            leds[trail_2] = colors.trail_2;
        }
    }

    // When the pulse reaches the center,
    // make the three center LEDs flash brighter
    if position == BRANCH_LENGTH - 1 {
        leds[0] = colors.center; // Red stick
        leds[15] = colors.center; // Green stick
        leds[16] = colors.center; // Blue stick
    }

    Ok(())
}

fn render_flux_communication(
    controller: &mut rs_ws281x::Controller,
    position: usize,
) -> Result<(), String> {
    let leds = controller.leds_mut(0);

    // Dark blue background
    for led in leds.iter_mut() {
        *led = [3, 10, 20, 0];
    }

    for branch in 0..BRANCH_COUNT {
        let base = branch * BRANCH_LENGTH;

        // Stick 3 is mounted in the opposite direction
        let current = if branch == 2 {
            base + (BRANCH_LENGTH - 1 - position)
        } else {
            base + position
        };

        // Bright communication pulse
        leds[current] = [180, 255, 255, 0];

        // First trail LED
        if position >= 1 {
            let trail_1 = if branch == 2 {
                base + (BRANCH_LENGTH - position)
            } else {
                base + position - 1
            };

            leds[trail_1] = [80, 200, 255, 0];
        }

        // Second trail LED
        if position >= 2 {
            let trail_2 = if branch == 2 {
                base + (BRANCH_LENGTH + 1 - position)
            } else {
                base + position - 2
            };

            leds[trail_2] = [30, 90, 160, 0];
        }
    }

    // Bright center flash
    if position == BRANCH_LENGTH - 1 {
        leds[7] = [255, 255, 255, 0];
        leds[15] = [255, 255, 255, 0];
        leds[16] = [255, 255, 255, 0];
    }

    Ok(())
}

pub(crate) fn ethernet_connected() -> bool {
    let interfaces = match std::fs::read_dir("/sys/class/net") {
        Ok(entries) => entries,
        Err(_) => return false,
    };

    for entry in interfaces.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();

        if name == "lo" {
            continue;
        }

        let carrier_path = entry.path().join("carrier");

        if let Ok(value) = std::fs::read_to_string(carrier_path) {
            if value.trim() == "1" {
                return true;
            }
        }
    }

    false
}

// For future implementation that will happen soon.
fn render_self_test_bar(controller: &mut rs_ws281x::Controller, mask: u32) {
    let leds = controller.leds_mut(1);

    for (i, led) in leds.iter_mut().enumerate() {
        let enabled = (mask & (1u32 << i)) != 0;

        *led = if enabled {
            [0, LEVELS[i], 0, 0]
        } else {
            [0; 4]
        };
    }
}

fn render_boot_blink(controller: &mut rs_ws281x::Controller) -> Result<(), String> {
    for _ in 0..2 {
        render_self_test_main(controller, 0x00FF_FFFF);

        controller
            .render()
            .map_err(|e| format!("Failed to render boot blink: {e:?}"))?;

        thread::sleep(Duration::from_millis(150));

        render_self_test_main(controller, 0);

        controller
            .render()
            .map_err(|e| format!("Failed to render boot blink: {e:?}"))?;

        thread::sleep(Duration::from_millis(150));
    }
    Ok(())
}

fn render_self_test_main(controller: &mut rs_ws281x::Controller, mask: u32) {
    let leds = controller.leds_mut(0);

    for (i, led) in leds.iter_mut().enumerate() {
        let enabled = (mask & (1u32 << i)) != 0;

        *led = if enabled {
            [0, LEVELS[i % 8], 0, 0]
        } else {
            [0; 4]
        };
    }
}

// Unit tests
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_blue_variant() {
        assert_eq!(parse_led_mode("blue"), LedMode::Blue);
    }

    #[test]
    fn test_green_variant() {
        assert_eq!(parse_led_mode("green"), LedMode::Green);
    }

    #[test]
    fn test_uppercase_variant() {
        assert_eq!(parse_led_mode("GREEN"), LedMode::Green);
    }

    #[test]
    fn test_invalid_variant_defaults_to_blue() {
        assert_eq!(parse_led_mode("invalid"), LedMode::Waiting);
    }

    #[test]
    fn test_led_mode_parsing() {
        assert_eq!(parse_led_mode("waiting"), LedMode::Waiting);
        assert_eq!(parse_led_mode("blue"), LedMode::Blue);
        assert_eq!(parse_led_mode("green"), LedMode::Green);
        assert_eq!(parse_led_mode("idle"), LedMode::Idle);
        assert_eq!(parse_led_mode("invalid"), LedMode::Waiting);
    }
}
