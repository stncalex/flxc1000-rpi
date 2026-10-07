use rs_ws281x::{ChannelBuilder, ControllerBuilder, StripType};

use std::{
    sync::{
        atomic::Ordering,
        Arc,
    },
    thread,
    time::Duration,
};

use super::SharedState;

const ETHERNET_LED_COUNT: i32 = 24;
const SELF_TEST_LED_COUNT: i32 = 8;

// Brightness levels corresponding to the old 8 LEDs
const LEVELS: [u8; 8] = [
    80,
    105,
    130,
    155,
    180,
    205,
    230,
    255,
];

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

        // RGB #1 - Ethernet status
        // GPIO18 = physical pin 12
        .channel(
            0,
            ChannelBuilder::new()
                .pin(18)
                .count(ETHERNET_LED_COUNT)
                .strip_type(StripType::Ws2812)
                .brightness(32)
                .build(),
        )

        // RGB #2 - replacement for old LED bar
        // GPIO13 = physical pin 33
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

    let mut last_connected: Option<bool> = None;
    let mut last_led_mask: u8 = 0xFF;

    loop {
        let connected = ethernet_connected();
        let led_mask = shared.rgb_led_mask.load(Ordering::Relaxed);

        if last_connected != Some(connected) || last_led_mask != led_mask {
            // ------------------------------------
            // RGB #1 - Ethernet status
            // ------------------------------------

            let ethernet_color = if connected {
                // Green
                [0, 255, 0, 0]
            } else {
                // Red
                [0, 0, 255, 0]
            };

            // for led in controller.leds_mut(0) {
            //     //*led = ethernet_color;
            // }
            

            for led in controller.leds_mut(0) {
                *led = ethernet_color;
            }

            // ------------------------------------
            // RGB #2 - old LED logic
            // ------------------------------------

            // let leds = controller.leds_mut(1);

            // // Turn all 8 pixels off first
            // for led in leds.iter_mut() {
            //     *led = [0, 0, 0, 0];
            // }

            // Logical LED 1 -> RGB pixel 1
            // Logical LED 2 -> RGB pixel 2
            // ...
            // Logical LED 8 -> RGB pixel 8
            // for i in 0..8 {
            //     if led_mask & (1 << i) != 0 {
            //         leds[i] = [0, LEVELS[i], 0, 0];
            //     }
            // }

            controller
                .render()
                .map_err(|e| format!("Failed to render RGB LEDs: {e:?}"))?;

            last_connected = Some(connected);
            last_led_mask = led_mask;
        }

        thread::sleep(Duration::from_millis(50));
    }
}

fn ethernet_connected() -> bool {
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