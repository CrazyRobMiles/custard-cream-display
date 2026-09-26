//! Polling driver for the GT911 capacitive touch controller (first touch point only).

use embedded_hal::i2c::I2c;

use super::board::GT911_ADDR;

const REG_PRODUCT_ID: u16 = 0x8140;
const REG_STATUS: u16 = 0x814E;
const REG_POINT1: u16 = 0x814F;

const STATUS_READY: u8 = 0x80;

pub enum Touch {
    /// The controller has no new data since the last poll.
    NoChange,
    Released,
    Pressed { x: u16, y: u16 },
}

fn read<B: I2c>(bus: &mut B, reg: u16, buf: &mut [u8]) -> Result<(), B::Error> {
    bus.write_read(GT911_ADDR, &reg.to_be_bytes(), buf)
}

fn write_u8<B: I2c>(bus: &mut B, reg: u16, value: u8) -> Result<(), B::Error> {
    let [hi, lo] = reg.to_be_bytes();
    bus.write(GT911_ADDR, &[hi, lo, value])
}

/// Returns the product ID ("911") if the controller answers.
pub fn product_id<B: I2c>(bus: &mut B) -> Result<[u8; 4], B::Error> {
    let mut id = [0u8; 4];
    read(bus, REG_PRODUCT_ID, &mut id)?;
    Ok(id)
}

pub fn poll<B: I2c>(bus: &mut B) -> Result<Touch, B::Error> {
    let mut status = [0u8];
    read(bus, REG_STATUS, &mut status)?;
    if status[0] & STATUS_READY == 0 {
        return Ok(Touch::NoChange);
    }

    let points = status[0] & 0x0F;
    let touch = if points == 0 {
        Touch::Released
    } else {
        // track id, x lo, x hi, y lo, y hi, size lo, size hi, reserved
        let mut p = [0u8; 8];
        read(bus, REG_POINT1, &mut p)?;
        Touch::Pressed {
            x: u16::from_le_bytes([p[1], p[2]]),
            y: u16::from_le_bytes([p[3], p[4]]),
        }
    };

    // Hand the buffer back to the controller.
    write_u8(bus, REG_STATUS, 0)?;
    Ok(touch)
}
