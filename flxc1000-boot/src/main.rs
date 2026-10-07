/*
 * SPDX-FileCopyrightText: 2026 Copyright (c) Contributors to the Eclipse Foundation
 *
 * See the NOTICE file(s) distributed with this work for additional
 * information regarding copyright ownership.
 *
 * This program and the accompanying materials are made available under the
 * terms of the Apache License Version 2.0 which is available at
 * https://www.apache.org/licenses/LICENSE-2.0
 *
 * SPDX-License-Identifier: Apache-2.0
 */

use std::ffi::CString;
use std::fs::File;
use std::os::unix::fs::PermissionsExt;
use std::io::Write;
use std::process;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use ace_server::config::*;
use ace_server::handler::ServerHandler;
use ace_server::nrc::BuiltinNrc;
use ace_server::security_provider::{SecurityError, SecurityProvider};
use ace_server::server::{UdsServer, MAX_FRAME, MAX_OUTBOX};
use ace_sim::clock::{Duration as AceDuration, Instant as AceInstant};
use ace_sim::io::NodeAddress;

use flxc1000_doip::server::{get_mac_address, DoipConfig, DoipServer};

const ECU_ADDRESS: u16 = 0x1000;
const VIN: &[u8; 17] = b"FLXC1000TEST00001";

/// Boot variant identification: DID 0xF100 = 0xFF0000
const VARIANT_ID: [u8; 3] = [0xFF, 0x00, 0x00];

const RESET_NONE: u8 = 0;
const RESET_HARD: u8 = 1;
const RESET_SOFT: u8 = 3;

//const UPDATE_FILE: &str = "app/update.tmp";

const APP_V1_BINARY: &str = "/app/flxc1000-app-v1";
const APP_V2_BINARY: &str = "/app/flxc1000-app-v2";
const BOOT_STATE_PATH: &str = "/data/boot_state";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppSlot {
    V1,
    V2,
}

impl AppSlot {
    fn binary_path(self) -> &'static str {
        match self {
            AppSlot::V1 => APP_V1_BINARY,
            AppSlot::V2 => APP_V2_BINARY,
        }
    }

    fn other(self) -> Self {
        match self {
            AppSlot::V1 => AppSlot::V2,
            AppSlot::V2 => AppSlot::V1,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            AppSlot::V1 => "V1",
            AppSlot::V2 => "V2",
        }
    }
}

#[derive(Debug)]
struct BootState {
    active: AppSlot, 
    candidate: Option<AppSlot>,
}

impl BootState {
    fn initial() -> Self {
        Self{
            active: AppSlot::V1,
            candidate: None,
        }
    }

    fn parse(contents: &str) -> Self {
        let mut state = Self::initial();

            for line in contents.lines() {
                if let Some(value) = line.strip_prefix("active=") {
                    state.active = match value {
                        "V1" => AppSlot::V1,
                        "V2" => AppSlot::V2,
                        _ => state.active,
                    };
                } else if let Some(value) = line.strip_prefix("candidate=") {
                    state.candidate = match value {
                        "V1" => Some(AppSlot::V1),
                        "V2" => Some(AppSlot::V2),
                        _ => None,
                    };
                }
            }
        state
    
}

fn to_text(&self) -> String {
    let candidate = match self.candidate {
        Some(slot) => slot.as_str(),
        None => "none",
    };

    format!(
        "active={}\ncandidate={}\nmode=application\n",
        self.active.as_str(),
        candidate
    )
}
}

// ---------------------------------------------------------------------------
// Shared state between handler (owned by UdsServer) and the main loop
// ---------------------------------------------------------------------------

struct SharedState {
    pending_reset: AtomicU8,
    session_type: AtomicU8,
}

// ---------------------------------------------------------------------------
// ServerHandler — application-level UDS hooks
// ---------------------------------------------------------------------------

struct BootHandler {
    shared: Arc<SharedState>,
    //transfer_buf: Vec<u8>,
    transfer_file: Option<File>,
    expected_size: usize,
    received_size: usize,
    target_slot: Option<AppSlot>,
}

impl ServerHandler for BootHandler {
    type Error = BuiltinNrc;

    fn read_did(&self, did: u16, buf: &mut [u8]) -> Result<usize, BuiltinNrc> {
        match did {
            0xF100 => {
                buf[..3].copy_from_slice(&VARIANT_ID);
                Ok(3)
            }
            0xF186 => {
                buf[0] = self.shared.session_type.load(Ordering::Relaxed);
                Ok(1)
            }
            _ => Err(BuiltinNrc::RequestOutOfRange),
        }
    }

    fn write_did(&mut self, _did: u16, _data: &[u8]) -> Result<(), BuiltinNrc> {
        Err(BuiltinNrc::RequestOutOfRange)
    }

    fn ecu_reset(&mut self, reset_type: u8) -> Result<(), BuiltinNrc> {
        match reset_type {
            0x01 => {
                tracing::info!("ECUReset HardReset — will re-exec /init after response");
                self.shared
                    .pending_reset
                    .store(RESET_HARD, Ordering::SeqCst);
                Ok(())
            }
            0x03 => {
                tracing::info!("ECUReset SoftReset — will restart boot after response");
                self.shared
                    .pending_reset
                    .store(RESET_SOFT, Ordering::SeqCst);
                Ok(())
            }
            _ => Err(BuiltinNrc::SubFunctionNotSupported),
        }
    }

    fn request_download(
        &mut self,
        _memory_address: &[u8],
        memory_size: &[u8],
        _compression_method: u8,
        _encrypting_method: u8,
        buf: &mut [u8],
    ) -> Result<usize, BuiltinNrc> {
        // Stage 1: stub positive response
        tracing::info!("RequestDownload: preparing update file");

        let expected_size = memory_size
            .iter()
            .fold(0usize, |size, &byte| (size << 8) | byte as usize);

        self.expected_size = expected_size;
        self.received_size = 0;

        // let file = File::create(UPDATE_FILE)
        //     .map_err(|e| {
        //         tracing::error!(error = %e, "Failed to create update file");
        //         BuiltinNrc::GeneralProgrammingFailure
        //     })?;

        let boot_state_contents = std::fs::read_to_string(BOOT_STATE_PATH)
            .map_err(|e| {
                tracing::error!(error = %e, "Failed to read boot state");
                BuiltinNrc::GeneralProgrammingFailure
            })?;

        let boot_state = BootState::parse(&boot_state_contents);

        let target_slot = boot_state.active.other();
        let target_path = target_slot.binary_path();
        self.target_slot = Some(target_slot);

        tracing::info!(
            active = ?boot_state.active,
            target = ?target_slot,
            path = target_path,
            "RequestDownload: selected inactive application slot"
        );

        let file = File::create(target_path)
            .map_err(|e| {
                tracing::error!(error = %e, "Failed to create inactibe application file");
                BuiltinNrc::GeneralProgrammingFailure
            })?;

        self.transfer_file = Some(file);
        // lengthFormatIdentifier: 0x20 (2 bytes for maxBlockLength)
        // maxNumberOfBlockLength: 0x0FFF (4095 bytes)
        buf[0] = 0x20;
        buf[1] = 0x0F;
        buf[2] = 0xFF;
        Ok(3)
    }

    fn transfer_data(
        &mut self,
        block_sequence_counter: u8,
        data: &[u8],
        buf: &mut [u8],
    ) -> Result<usize, BuiltinNrc> {
        // Stage 1: stub — accumulate bytes
        //self.transfer_buf.extend_from_slice(data);
        let file = self 
            .transfer_file
            .as_mut()
            .ok_or(BuiltinNrc::RequestSequenceError)?;

        file.write_all(data).map_err(|e| {
            tracing::error!(error = %e, "Failed to write update data");
            BuiltinNrc::GeneralProgrammingFailure
        })?;

        self.received_size += data.len();

        tracing::info!(
            block = block_sequence_counter,
            bytes = data.len(),
            //total = self.transfer_buf.len(),
            "TransferData: wrote block to update file"
        );
        buf[0] = block_sequence_counter;
        Ok(1)
    }

    fn request_transfer_exit(
        &mut self,
        _parameter_record: &[u8],
        _buf: &mut [u8],
    ) -> Result<usize, BuiltinNrc> {
        let file = self 
            .transfer_file
            .take()
            .ok_or(BuiltinNrc::RequestSequenceError)?;

        if self.received_size != self.expected_size {
            tracing::error!(
                expected = self.expected_size,
                received = self.received_size,
                "Transfer size mismatch"
            );

            return Err(BuiltinNrc::GeneralProgrammingFailure);
        }
        
        file.sync_all().map_err(|e| {
            tracing::error!(error = %e, "Failed to sync update file"); 
            BuiltinNrc::GeneralProgrammingFailure
        })?;

        let target_slot = self
            .target_slot
            .take()
            .ok_or(BuiltinNrc::RequestSequenceError)?;

        let target_path = target_slot.binary_path();

        let permissions = std::fs::Permissions::from_mode(0o755);

        std::fs::set_permissions(target_path, permissions)
            .map_err(|e| {
                tracing::error!(error = %e, "Failed to make application executable");
                BuiltinNrc::GeneralProgrammingFailure
            })?;

        let boot_state_contents = std::fs::read_to_string(BOOT_STATE_PATH)
            .map_err(|e| {
                tracing::error!(error = %e, "Failed to read about boot state");
                BuiltinNrc::GeneralProgrammingFailure
            })?;

        let mut boot_state = BootState::parse(&boot_state_contents);

        boot_state.candidate = Some(target_slot);

        std::fs::write(BOOT_STATE_PATH, boot_state.to_text())
            .map_err(|e| {
                tracing::error!(error = %e, "Failed to update boot state");
                BuiltinNrc::GeneralProgrammingFailure
            })?;

        tracing::info!("TransferExit: update file successfully written");
        //tracing::info!(total_bytes = self.transfer_buf.len(), "TransferExit (stub)");
        //self.transfer_buf.clear();
        Ok(0)
    }
}

// ---------------------------------------------------------------------------
// SecurityProvider — seed/key with XOR 0xDEADBEEF
// ---------------------------------------------------------------------------

struct BootSecurity;

impl SecurityProvider for BootSecurity {
    fn generate_seed(&mut self, _level: u8, buf: &mut [u8]) -> Result<usize, SecurityError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u32;
        let seed = now.wrapping_mul(2654435761); // Knuth multiplicative hash
        buf[..4].copy_from_slice(&seed.to_be_bytes());
        Ok(4)
    }

    fn validate_key(&self, _level: u8, seed: &[u8], key: &[u8]) -> Result<(), SecurityError> {
        if seed.len() < 4 || key.len() < 4 {
            return Err(SecurityError::InvalidKey);
        }
        let seed_u32 = u32::from_be_bytes([seed[0], seed[1], seed[2], seed[3]]);
        let expected = seed_u32 ^ 0xDEAD_BEEF;
        let actual = u32::from_be_bytes([key[0], key[1], key[2], key[3]]);
        if actual == expected {
            Ok(())
        } else {
            Err(SecurityError::InvalidKey)
        }
    }
}

// ---------------------------------------------------------------------------
// Build the ace-server configuration for the Boot variant
// ---------------------------------------------------------------------------

fn boot_server_config() -> ServerConfig {
    ServerConfig::new(ECU_ADDRESS, 0xFFFF)
        // Sessions
        .with_session(SessionConfig::default_session())
        .with_session(SessionConfig::programming_session())
        .with_session(SessionConfig::extended_session())
        // Services: (SID, allowed sessions)
        .with_service(ServiceConfig::new(0x10, &[0x01, 0x02, 0x03])) // DiagnosticSessionControl
        .with_service(ServiceConfig::new(0x11, &[0x01, 0x02, 0x03])) // ECUReset
        .with_service(ServiceConfig::new(0x22, &[0x01, 0x02, 0x03])) // ReadDataByIdentifier
        .with_service(ServiceConfig::new(0x27, &[0x02, 0x03])) // SecurityAccess
        .with_service(ServiceConfig::secured(0x34, &[0x02], 0x03)) // RequestDownload (programming, sec 03)
        .with_service(ServiceConfig::secured(0x36, &[0x02], 0x03)) // TransferData
        .with_service(ServiceConfig::secured(0x37, &[0x02], 0x03)) // RequestTransferExit
        .with_service(ServiceConfig::new(0x3E, &[0x01, 0x02, 0x03])) // TesterPresent
        // DIDs
        .with_did(DidConfig::read_only(0xF100, &[0x01, 0x02, 0x03])) // variant ID
        .with_did(DidConfig::read_only(0xF186, &[0x01, 0x02, 0x03])) // session
        // Security levels
        .with_security_level(SecurityLevelConfig {
            level: 0x03,
            max_attempts: 3,
            lockout_duration: AceDuration::from_secs(10),
            seed_length: 4,
            key_length: 4,
        })
}

// ---------------------------------------------------------------------------
// Reset helpers
// ---------------------------------------------------------------------------

fn do_hard_reset() -> ! {
    let c_path = CString::new("/init").expect("invalid path");
    let argv = [c_path.clone()];
    let env: Vec<CString> = vec![
        CString::new("PATH=/bin:/sbin").unwrap(),
        CString::new("HOME=/").unwrap(),
    ];
    match nix::unistd::execve(&c_path, &argv, &env) {
        Ok(infallible) => match infallible {},
        Err(e) => {
            tracing::error!("execve /init failed: {}", e);
            process::exit(1);
        }
    }
}

fn do_soft_reset() -> ! {
    let exe = std::env::current_exe().unwrap_or_else(|_| "/boot/flxc1000-boot".into());
    let c_path = CString::new(exe.to_string_lossy().as_ref()).expect("invalid path");
    let argv = [c_path.clone()];
    let env: Vec<CString> = vec![
        CString::new("PATH=/bin:/sbin").unwrap(),
        CString::new("HOME=/").unwrap(),
    ];
    match nix::unistd::execve(&c_path, &argv, &env) {
        Ok(infallible) => match infallible {},
        Err(e) => {
            tracing::error!("execve self failed: {}", e);
            process::exit(1);
        }
    }
}

// ---------------------------------------------------------------------------
// ace_sim::clock helpers — convert std::time to ace Instant
// ---------------------------------------------------------------------------

fn ace_now(boot: &std::time::Instant) -> AceInstant {
    AceInstant::from_micros(boot.elapsed().as_micros() as u64)
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    tracing::info!(
        "flxc1000-boot starting (Boot variant, address 0x{:04X})",
        ECU_ADDRESS
    );

    let mac = get_mac_address();
    tracing::info!(
        "EID (MAC): {:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        mac[0],
        mac[1],
        mac[2],
        mac[3],
        mac[4],
        mac[5]
    );

    let doip_config = DoipConfig::new(ECU_ADDRESS, VIN).with_eid(mac);
    let doip = DoipServer::new(doip_config);
    let mut uds_rx = doip.run().await.expect("Failed to start DoIP server");

    let shared = Arc::new(SharedState {
        pending_reset: AtomicU8::new(RESET_NONE),
        session_type: AtomicU8::new(0x01),
    });

    let handler = BootHandler {
        shared: Arc::clone(&shared),
        transfer_file: None,
        expected_size: 0,
        received_size: 0,
        target_slot: None,
    };

    let mut server = UdsServer::new(
        boot_server_config(),
        handler,
        BootSecurity,
        NodeAddress(ECU_ADDRESS as u32),
    );

    let boot_time = std::time::Instant::now();
    tracing::info!("Boot ECU ready, waiting for diagnostic requests");

    while let Some(req) = uds_rx.recv().await {
        tracing::debug!(
            src = format_args!("0x{:04X}", req.source_address),
            tgt = format_args!("0x{:04X}", req.target_address),
            data = format_args!("{:02X?}", &req.data),
            "UDS request"
        );

        let now = ace_now(&boot_time);

        // Keep session_type in shared state up to date for DID 0xF186
        shared
            .session_type
            .store(server.session_type(), Ordering::Relaxed);

        let src = NodeAddress(req.source_address as u32);
        let _ = server.handle(&src, &req.data, now);
        let _ = server.tick(now);

        // Update session_type after handle (may have changed)
        shared
            .session_type
            .store(server.session_type(), Ordering::Relaxed);

        // Drain outbox → collect response for this tester
        let mut outbox: heapless::Vec<(NodeAddress, heapless::Vec<u8, MAX_FRAME>), MAX_OUTBOX> =
            heapless::Vec::new();
        server.drain_outbox(&mut outbox);

        let response: Vec<u8> = outbox
            .into_iter()
            .find(|(dst, _)| dst == &src)
            .map(|(_, data)| data.to_vec())
            .unwrap_or_default();

        tracing::debug!(data = format_args!("{:02X?}", &response), "UDS response");

        if let Err(e) = req.response_tx.send(response).await {
            tracing::error!("Failed to send UDS response: {}", e);
        }

        // Check for pending reset AFTER response is sent
        let reset = shared.pending_reset.swap(RESET_NONE, Ordering::SeqCst);
        match reset {
            RESET_HARD => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                do_hard_reset();
            }
            RESET_SOFT => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                do_soft_reset();
            }
            _ => {}
        }
    }
}
