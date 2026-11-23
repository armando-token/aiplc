use thiserror::Error;

#[derive(Debug, Error)]
pub enum WasmInspectError {
    #[error("invalid wasm magic or version")]
    BadHeader,
    #[error("unexpected EOF parsing wasm section")]
    Truncated,
    #[error("forbidden start section (section 8) present")]
    ForbiddenStartSection,
    #[error("forbidden imports present (no WASI or host imports allowed)")]
    ForbiddenImports,
    #[error("linear memory pages ({max_pages}) exceed limit ({limit_pages})")]
    MemoryLimitExceeded { max_pages: u32, limit_pages: u32 },
    #[error("shared/thread memory forbidden")]
    ForbiddenSharedMemory,
    #[error("missing required export '{0}'")]
    MissingExport(&'static str),
}

fn read_leb_u32(data: &[u8], pos: &mut usize) -> Result<u32, WasmInspectError> {
    let mut res: u32 = 0;
    let mut shift = 0u32;
    loop {
        if *pos >= data.len() {
            return Err(WasmInspectError::Truncated);
        }
        let b = data[*pos];
        *pos += 1;
        res |= ((b & 0x7f) as u32) << shift;
        if (b & 0x80) == 0 {
            return Ok(res);
        }
        shift += 7;
        if shift >= 35 {
            return Err(WasmInspectError::Truncated);
        }
    }
}

/// Validates a WebAssembly binary offline prior to loading into WAMR/Wasmtime (§4.2):
/// - Checks `\0asm` v1 magic
/// - Rejects any imports (blocks WASI, filesystem, network, clock, threads)
/// - Rejects `start` section (section 8)
/// - Verifies memory section does not exceed `max_linear_mem_bytes` and is non-shared
/// - Confirms `eval_scan` export is present
pub fn validate_wasm_module_bytes(
    bytes: &[u8],
    max_linear_mem_bytes: u32,
) -> Result<(), WasmInspectError> {
    if bytes.len() < 8 || bytes[0..8] != [0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00] {
        return Err(WasmInspectError::BadHeader);
    }
    let limit_pages = (max_linear_mem_bytes / 65536).max(1);
    let mut pos = 8usize;
    let mut has_eval_scan = false;

    while pos < bytes.len() {
        let sec_id = bytes[pos];
        pos += 1;
        let sec_len = read_leb_u32(bytes, &mut pos)? as usize;
        let sec_end = pos.checked_add(sec_len).ok_or(WasmInspectError::Truncated)?;
        if sec_end > bytes.len() {
            return Err(WasmInspectError::Truncated);
        }
        let sec = &bytes[pos..sec_end];
        pos = sec_end;

        match sec_id {
            2 => {
                // Import section: count must be 0
                let mut sp = 0usize;
                let count = read_leb_u32(sec, &mut sp)?;
                if count > 0 {
                    return Err(WasmInspectError::ForbiddenImports);
                }
            }
            5 => {
                // Memory section
                let mut sp = 0usize;
                let count = read_leb_u32(sec, &mut sp)?;
                for _ in 0..count {
                    if sp >= sec.len() {
                        return Err(WasmInspectError::Truncated);
                    }
                    let flags = sec[sp];
                    sp += 1;
                    if (flags & 0x02) != 0 {
                        return Err(WasmInspectError::ForbiddenSharedMemory);
                    }
                    let initial = read_leb_u32(sec, &mut sp)?;
                    let max_pages = if (flags & 0x01) != 0 {
                        read_leb_u32(sec, &mut sp)?
                    } else {
                        // Unbounded Wasm linear memory permits unbounded grow (DEFAULT_MAX_PAGES = 65536) -> reject
                        65_536
                    };
                    if initial > limit_pages || max_pages > limit_pages {
                        return Err(WasmInspectError::MemoryLimitExceeded {
                            max_pages: initial.max(max_pages),
                            limit_pages,
                        });
                    }
                }
            }
            7 => {
                // Export section: look for "eval_scan"
                let mut sp = 0usize;
                let count = read_leb_u32(sec, &mut sp)?;
                for _ in 0..count {
                    let name_len = read_leb_u32(sec, &mut sp)? as usize;
                    let name_end = sp.checked_add(name_len).ok_or(WasmInspectError::Truncated)?;
                    let kind_pos = name_end.checked_add(1).ok_or(WasmInspectError::Truncated)?;
                    if kind_pos > sec.len() {
                        return Err(WasmInspectError::Truncated);
                    }
                    let name = &sec[sp..name_end];
                    let kind = sec[name_end];
                    sp = kind_pos;
                    let _idx = read_leb_u32(sec, &mut sp)?;
                    if kind == 0 && name == b"eval_scan" {
                        has_eval_scan = true;
                    }
                }
            }
            8 => {
                return Err(WasmInspectError::ForbiddenStartSection);
            }
            _ => {}
        }
    }

    if !has_eval_scan {
        return Err(WasmInspectError::MissingExport("eval_scan"));
    }
    Ok(())
}

/// Validates WAMR AOT magic header (`\0aot`). Must still pass Ed25519 signature + aot_digest check!
pub fn validate_aot_magic(bytes: &[u8]) -> Result<(), WasmInspectError> {
    if bytes.len() < 8 || bytes[0..4] != [0x00, 0x61, 0x6f, 0x74] {
        return Err(WasmInspectError::BadHeader);
    }
    Ok(())
}
