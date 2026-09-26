//! Minimal driver for the CH422G I/O expander.
//!
//! The CH422G has no register pointer; each "register" is its own I2C address.
//! Writing `0x01` to the system-parameter address enables the IO0-IO7 outputs,
//! and writing a byte to the output address sets all eight pins at once.

use core::cell::{Cell, RefCell};

use embedded_hal::digital::{ErrorKind, ErrorType, OutputPin};
use embedded_hal::i2c::I2c;

const ADDR_SYSTEM: u8 = 0x24;
const ADDR_OUTPUT: u8 = 0x38;
const SYSTEM_IO_OE: u8 = 0x01;

pub struct Ch422g<'a, B> {
    bus: &'a RefCell<B>,
    outputs: Cell<u8>,
}

impl<'a, B: I2c> Ch422g<'a, B> {
    /// Switches IO0-IO7 to push-pull outputs, driven to `initial`.
    pub fn new(bus: &'a RefCell<B>, initial: u8) -> Result<Self, B::Error> {
        {
            let mut bus = bus.borrow_mut();
            bus.write(ADDR_SYSTEM, &[SYSTEM_IO_OE])?;
            bus.write(ADDR_OUTPUT, &[initial])?;
        }
        Ok(Self {
            bus,
            outputs: Cell::new(initial),
        })
    }

    pub fn set(&self, pin: u8, high: bool) -> Result<(), B::Error> {
        let outputs = if high {
            self.outputs.get() | 1 << pin
        } else {
            self.outputs.get() & !(1 << pin)
        };
        self.outputs.set(outputs);
        self.bus.borrow_mut().write(ADDR_OUTPUT, &[outputs])
    }

    /// One expander output as an `embedded-hal` pin, e.g. for a chip select.
    pub fn pin(&self, pin: u8) -> ExpanderPin<'_, 'a, B> {
        ExpanderPin {
            expander: self,
            pin,
        }
    }
}

pub struct ExpanderPin<'e, 'a, B> {
    expander: &'e Ch422g<'a, B>,
    pin: u8,
}

impl<B> ErrorType for ExpanderPin<'_, '_, B> {
    type Error = ErrorKind;
}

impl<B: I2c> OutputPin for ExpanderPin<'_, '_, B> {
    fn set_low(&mut self) -> Result<(), Self::Error> {
        self.expander
            .set(self.pin, false)
            .map_err(|_| ErrorKind::Other)
    }

    fn set_high(&mut self) -> Result<(), Self::Error> {
        self.expander
            .set(self.pin, true)
            .map_err(|_| ErrorKind::Other)
    }
}
