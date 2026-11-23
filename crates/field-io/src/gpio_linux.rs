use std::os::fd::RawFd;
use thiserror::Error;

pub const GPIO_MAX_LINES: usize = 64;
pub const GPIO_MAX_NAME_SIZE: usize = 32;
pub const GPIO_V2_NUM_ATTRS_MAX: usize = 10;

pub const GPIO_V2_LINE_FLAG_ACTIVE_LOW: u64 = 1 << 1;
pub const GPIO_V2_LINE_FLAG_INPUT: u64 = 1 << 2;
pub const GPIO_V2_LINE_FLAG_OUTPUT: u64 = 1 << 3;

// Verified directly on Luckfox Lyra Linux 6.1.99 armv7l (<linux/gpio.h>)
pub const GPIO_V2_GET_LINE_IOCTL: libc::c_ulong = 0xc250b407;
pub const GPIO_V2_LINE_SET_VALUES_IOCTL: libc::c_ulong = 0xc010b40f;
pub const GPIO_V2_LINE_GET_VALUES_IOCTL: libc::c_ulong = 0xc010b40e;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GpioV2LineAttribute {
    pub id: u32,
    pub padding: u32,
    pub value: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GpioV2LineConfigAttribute {
    pub attr: GpioV2LineAttribute,
    pub mask: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GpioV2LineConfig {
    pub flags: u64,
    pub num_attrs: u32,
    pub padding: [u32; 5],
    pub attrs: [GpioV2LineConfigAttribute; GPIO_V2_NUM_ATTRS_MAX],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GpioV2LineRequest {
    pub offsets: [u32; GPIO_MAX_LINES],
    pub consumer: [u8; GPIO_MAX_NAME_SIZE],
    pub config: GpioV2LineConfig,
    pub num_lines: u32,
    pub event_buffer_size: u32,
    pub padding: [u32; 5],
    pub fd: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GpioV2LineValues {
    pub bits: u64,
    pub mask: u64,
}

#[derive(Debug, Error)]
pub enum GpioError {
    #[error("GPIO ioctl/open failed: {0}")]
    Os(#[from] std::io::Error),
    #[error("invalid line count {0}")]
    InvalidLineCount(usize),
    #[error("unsupported on non-Linux platform")]
    UnsupportedPlatform,
}

#[derive(Debug, Clone)]
pub struct GpioLineSpec {
    pub offset: u32,
    pub active_low: bool,
}

/// Exclusive owner of a group of GPIO output lines via Linux GPIO chardev v2.
/// Forces all owned lines to 0 (safe de-energized state) on Drop.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct GpioChardevV2 {
    chip_fd: RawFd,
    line_fd: RawFd,
    num_lines: usize,
    mask_all: u64,
}

impl GpioChardevV2 {
    #[cfg(target_os = "linux")]
    pub fn open_outputs(
        chip_path: &str,
        consumer: &str,
        lines: &[GpioLineSpec],
    ) -> Result<Self, GpioError> {
        use std::ffi::CString;
        if lines.is_empty() || lines.len() > GPIO_MAX_LINES {
            return Err(GpioError::InvalidLineCount(lines.len()));
        }
        let c_path = CString::new(chip_path)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "nul in path"))?;
        let chip_fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if chip_fd < 0 {
            return Err(GpioError::Os(std::io::Error::last_os_error()));
        }

        let mut req: GpioV2LineRequest = unsafe { std::mem::zeroed() };
        for (idx, line) in lines.iter().enumerate() {
            req.offsets[idx] = line.offset;
        }
        let c_bytes = consumer.as_bytes();
        let copy_len = c_bytes.len().min(GPIO_MAX_NAME_SIZE - 1);
        req.consumer[..copy_len].copy_from_slice(&c_bytes[..copy_len]);
        req.num_lines = lines.len() as u32;
        req.config.flags = GPIO_V2_LINE_FLAG_OUTPUT;

        // Apply per-line active-low flags if requested
        let mut active_low_mask = 0u64;
        for (idx, line) in lines.iter().enumerate() {
            if line.active_low {
                active_low_mask |= 1u64 << idx;
            }
        }
        let mask_all = if lines.len() == 64 {
            u64::MAX
        } else {
            (1u64 << lines.len()) - 1
        };

        // Initial output value = 0 (safe state) via attribute id=2 (GPIO_V2_LINE_ATTR_ID_OUTPUT_VALUES)
        req.config.attrs[0].attr.id = 2;
        req.config.attrs[0].attr.value = 0;
        req.config.attrs[0].mask = mask_all;
        req.config.num_attrs = 1;

        if active_low_mask != 0 {
            req.config.attrs[1].attr.id = 1; // GPIO_V2_LINE_ATTR_ID_FLAGS
            req.config.attrs[1].attr.value = GPIO_V2_LINE_FLAG_OUTPUT | GPIO_V2_LINE_FLAG_ACTIVE_LOW;
            req.config.attrs[1].mask = active_low_mask;
            req.config.num_attrs = 2;
        }

        let rc = unsafe {
            libc::ioctl(
                chip_fd,
                GPIO_V2_GET_LINE_IOCTL as libc::c_ulong,
                &mut req as *mut GpioV2LineRequest,
            )
        };
        if rc < 0 || req.fd < 0 {
            let err = if rc < 0 {
                std::io::Error::last_os_error()
            } else {
                std::io::Error::new(std::io::ErrorKind::Other, "invalid negative GPIO line fd")
            };
            unsafe { libc::close(chip_fd) };
            return Err(GpioError::Os(err));
        }

        Ok(Self {
            chip_fd,
            line_fd: req.fd,
            num_lines: lines.len(),
            mask_all,
        })
    }

    #[cfg(not(target_os = "linux"))]
    pub fn open_outputs(
        _chip_path: &str,
        _consumer: &str,
        _lines: &[GpioLineSpec],
    ) -> Result<Self, GpioError> {
        Err(GpioError::UnsupportedPlatform)
    }

    #[cfg(target_os = "linux")]
    pub fn set_values(&self, bits: u64) -> Result<(), GpioError> {
        let mut vals = GpioV2LineValues {
            bits: bits & self.mask_all,
            mask: self.mask_all,
        };
        let rc = unsafe {
            libc::ioctl(
                self.line_fd,
                GPIO_V2_LINE_SET_VALUES_IOCTL as libc::c_ulong,
                &mut vals as *mut GpioV2LineValues,
            )
        };
        if rc < 0 {
            return Err(GpioError::Os(std::io::Error::last_os_error()));
        }
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    pub fn set_values(&self, _bits: u64) -> Result<(), GpioError> {
        Err(GpioError::UnsupportedPlatform)
    }

    #[cfg(target_os = "linux")]
    pub fn get_values(&self) -> Result<u64, GpioError> {
        let mut vals = GpioV2LineValues {
            bits: 0,
            mask: self.mask_all,
        };
        let rc = unsafe {
            libc::ioctl(
                self.line_fd,
                GPIO_V2_LINE_GET_VALUES_IOCTL as libc::c_ulong,
                &mut vals as *mut GpioV2LineValues,
            )
        };
        if rc < 0 {
            return Err(GpioError::Os(std::io::Error::last_os_error()));
        }
        Ok(vals.bits & self.mask_all)
    }

    #[cfg(not(target_os = "linux"))]
    pub fn get_values(&self) -> Result<u64, GpioError> {
        Err(GpioError::UnsupportedPlatform)
    }

    pub fn line_count(&self) -> usize {
        self.num_lines
    }
}

impl Drop for GpioChardevV2 {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        {
            // Force de-energized safe state on shutdown/drop
            let _ = self.set_values(0);
            unsafe {
                libc::close(self.line_fd);
                libc::close(self.chip_fd);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    #[test]
    fn kernel_v2_struct_layout_matches_linux_uapi() {
        assert_eq!(size_of::<GpioV2LineAttribute>(), 16);
        assert_eq!(size_of::<GpioV2LineConfigAttribute>(), 24);
        assert_eq!(size_of::<GpioV2LineConfig>(), 272);
        assert_eq!(offset_of!(GpioV2LineConfig, flags), 0);
        assert_eq!(offset_of!(GpioV2LineConfig, num_attrs), 8);
        assert_eq!(offset_of!(GpioV2LineConfig, attrs), 32);
        assert_eq!(size_of::<GpioV2LineRequest>(), 592);
        assert_eq!(offset_of!(GpioV2LineRequest, offsets), 0);
        assert_eq!(offset_of!(GpioV2LineRequest, consumer), 256);
        assert_eq!(offset_of!(GpioV2LineRequest, config), 288);
        assert_eq!(offset_of!(GpioV2LineRequest, num_lines), 560);
        assert_eq!(offset_of!(GpioV2LineRequest, event_buffer_size), 564);
        assert_eq!(offset_of!(GpioV2LineRequest, fd), 588);
        assert_eq!(size_of::<GpioV2LineValues>(), 16);
    }
}
