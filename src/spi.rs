/*!
  # Serial Peripheral Interface
  To construct the SPI instances, use the `Spi::spiX` functions.

  The pin parameter is a tuple containing `(sck, miso, mosi)` which should be configured as `(Alternate<...>, Input<...>, Alternate<...>)`.
  As some STM32F1xx chips have 5V tolerant SPI pins, it is also possible to configure Sck and Mosi outputs as `Alternate<PushPull>`. Then
  a simple Pull-Up to 5V can be used to use SPI on a 5V bus without a level shifter.

  You can also use `NoSck`, `NoMiso` or `NoMosi` if you don't want to use the pins

  - `SPI1` can use `(PA5, PA6, PA7)` or `(PB3, PB4, PB5)`.
  - `SPI2` can use `(PB13, PB14, PB15)`
  - `SPI3` can use `(PB3, PB4, PB5)` or only in connectivity line devices `(PC10, PC11, PC12)`


  ## Initialisation example

  ```rust
    // Acquire the GPIOB peripheral
    let mut gpiob = dp.GPIOB.split();

    let pins = (
        gpiob.pb13.into_alternate_push_pull(&mut gpiob.crh),
        gpiob.pb14.into_floating_input(&mut gpiob.crh),
        gpiob.pb15.into_alternate_push_pull(&mut gpiob.crh),
    );

    let spi_mode = Mode {
        polarity: Polarity::IdleLow,
        phase: Phase::CaptureOnFirstTransition,
    };
    let spi = Spi::spi2(dp.SPI2, pins, spi_mode, 100.khz(), clocks);
  ```
*/

use core::ops::{Deref, DerefMut};
use core::ptr;

pub use crate::hal::spi::{FullDuplex, Mode, Phase, Polarity};
use crate::pac::{self, RCC};

use crate::afio::MAPR;
use crate::dma::dma1;
#[cfg(feature = "connectivity")]
use crate::dma::dma2;
use crate::dma::{Receive, RxDma, RxTxDma, Transfer, TransferPayload, Transmit, TxDma, R, W};
use crate::gpio::{self, Alternate, Cr, Floating, Input, PinMode, PullUp, PushPull};
use crate::rcc::{BusClock, Clocks, Enable, Reset};
use crate::time::Hertz;

use core::sync::atomic::{self, Ordering};
use embedded_dma::{ReadBuffer, WriteBuffer};

/// Interrupt event
pub enum Event {
    /// New data has been received
    Rxne,
    /// New data can be sent
    Txe,
    /// an error condition occurs(Crcerr, Overrun, ModeFault)
    Error,
}

/// SPI error
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// Overrun occurred
    Overrun,
    /// Mode fault occurred
    ModeFault,
    /// CRC error
    Crc,
}

use core::marker::PhantomData;

pub trait InMode {}
impl InMode for Floating {}
impl InMode for PullUp {}

pub mod spi1 {
    use super::*;

    remap! {
        MasterPins, SlavePins: [
            No, PA5, PA6, PA7 => MAPR { |_, w| w.spi1_remap().bit(false) };
            Remap, PB3, PB4, PB5  => MAPR { |_, w| w.spi1_remap().bit(true) };
        ]
    }
}

pub mod spi2 {
    use super::*;

    remap! {
        MasterPins, SlavePins: [
            No, PB13, PB14, PB15;
        ]
    }
}
#[cfg(any(feature = "high", feature = "connectivity"))]
pub mod spi3 {
    use super::*;

    remap! {
        MasterPins, SlavePins: [
            #[cfg(not(feature = "connectivity"))]
            No, PB3, PB4, PB5;
            #[cfg(feature = "connectivity")]
            No, PB3, PB4, PB5 => MAPR { |_, w| w.spi3_remap().bit(false) };
            #[cfg(feature = "connectivity")]
            Remap, PC10, PC11, PC12  => MAPR { |_, w| w.spi3_remap().bit(true) };
        ]
    }
}

macro_rules! remap {
    ($master:ident, $slave:ident: [
        $($(#[$attr:meta])* $rname:ident, $SCK:ident, $MISO:ident, $MOSI:ident $( => $MAPR:ident { $remapex:expr })?;)+
    ]) => {
        pub enum $master<INMODE> {
            $(
                $(#[$attr])*
                $rname { sck: gpio::$SCK<Alternate>, miso: gpio::$MISO<Input<INMODE>>, mosi: gpio::$MOSI<Alternate> },
            )+
        }

        pub enum $slave<OUTMODE, INMODE> {
            $(
                $(#[$attr])*
                $rname { sck: gpio::$SCK<Alternate>, miso: gpio::$MISO<Alternate<OUTMODE>>, mosi: gpio::$MOSI<Input<INMODE>> },
            )+
        }

        $(
            $(#[$attr])*
            impl<INMODE: InMode> From<(gpio::$SCK<Alternate>, gpio::$MISO<Input<INMODE>>, gpio::$MOSI<Alternate> $(, &mut $MAPR)?)> for $master<INMODE> {
                fn from(p: (gpio::$SCK<Alternate>, gpio::$MISO<Input<INMODE>>, gpio::$MOSI<Alternate> $(, &mut $MAPR)?)) -> Self {
                    $(p.3.modify_mapr($remapex);)?
                    Self::$rname { sck: p.0, miso: p.1, mosi: p.2 }
                }
            }

            $(#[$attr])*
            impl<INMODE> From<(gpio::$SCK, gpio::$MISO, gpio::$MOSI $(, &mut $MAPR)?)> for $master<INMODE>
            where
                Input<INMODE>: PinMode,
                INMODE: InMode,
            {
                fn from(p: (gpio::$SCK, gpio::$MISO, gpio::$MOSI $(, &mut $MAPR)?)) -> Self {
                    $(p.3.modify_mapr($remapex);)?
                    let mut cr = Cr::new();
                    Self::$rname { sck: p.0.into_mode(&mut cr), miso: p.1.into_mode(&mut cr), mosi: p.2.into_mode(&mut cr) }
                }
            }

            $(#[$attr])*
            impl<OUTMODE, INMODE: InMode> From<(gpio::$SCK<Alternate>, gpio::$MISO<Alternate<OUTMODE>>, gpio::$MOSI<Input<INMODE>> $(, &mut $MAPR)?)> for $slave<OUTMODE, INMODE> {
                fn from(p: (gpio::$SCK<Alternate>, gpio::$MISO<Alternate<OUTMODE>>, gpio::$MOSI<Input<INMODE>> $(, &mut $MAPR)?)) -> Self {
                    $(p.3.modify_mapr($remapex);)?
                    Self::$rname { sck: p.0, miso: p.1, mosi: p.2 }
                }
            }

            $(#[$attr])*
            impl<OUTMODE, INMODE> From<(gpio::$SCK, gpio::$MISO, gpio::$MOSI $(, &mut $MAPR)?)> for $slave<OUTMODE, INMODE>
            where
                Alternate<OUTMODE>: PinMode,
                Input<INMODE>: PinMode,
                INMODE: InMode,
            {
                fn from(p: (gpio::$SCK, gpio::$MISO, gpio::$MOSI $(, &mut $MAPR)?)) -> Self {
                    $(p.3.modify_mapr($remapex);)?
                    let mut cr = Cr::new();
                    Self::$rname { sck: p.0.into_mode(&mut cr), miso: p.1.into_mode(&mut cr), mosi: p.2.into_mode(&mut cr) }
                }
            }
        )+
    }
}
use remap;

pub struct SpiInner<SPI, FRAMESIZE> {
    spi: SPI,
    _framesize: PhantomData<FRAMESIZE>,
}

impl<SPI, FRAMESIZE> SpiInner<SPI, FRAMESIZE> {
    fn new(spi: SPI) -> Self {
        Self {
            spi,
            _framesize: PhantomData,
        }
    }
}

/// Spi in Master mode
pub struct Spi<SPI: Instance, FRAMESIZE, INMODE = Floating> {
    inner: SpiInner<SPI, FRAMESIZE>,
    pins: SPI::MasterPins<INMODE>,
}

/// Spi in Slave mode
pub struct SpiSlave<SPI: Instance, FRAMESIZE, OUTMODE = PushPull, INMODE = Floating> {
    inner: SpiInner<SPI, FRAMESIZE>,
    pins: SPI::SlavePins<OUTMODE, INMODE>,
}

impl<SPI: Instance, FRAMESIZE, INMODE> Deref for Spi<SPI, FRAMESIZE, INMODE> {
    type Target = SpiInner<SPI, FRAMESIZE>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<SPI: Instance, FRAMESIZE, INMODE> DerefMut for Spi<SPI, FRAMESIZE, INMODE> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<SPI: Instance, FRAMESIZE, OUTMODE, INMODE> Deref
    for SpiSlave<SPI, FRAMESIZE, OUTMODE, INMODE>
{
    type Target = SpiInner<SPI, FRAMESIZE>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<SPI: Instance, FRAMESIZE, OUTMODE, INMODE> DerefMut
    for SpiSlave<SPI, FRAMESIZE, OUTMODE, INMODE>
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

/// The bit format to send the data in
#[derive(Debug, Clone, Copy)]
pub enum SpiBitFormat {
    /// Least significant bit first
    LsbFirst,
    /// Most significant bit first
    MsbFirst,
}
/*
/// A filler type for when the SCK pin is unnecessary
pub struct NoSck;
/// A filler type for when the Miso pin is unnecessary
pub struct NoMiso;
/// A filler type for when the Mosi pin is unnecessary
pub struct NoMosi;

impl<REMAP> Sck<REMAP> for NoSck {}
impl<REMAP> Miso<REMAP> for NoMiso {}
impl<REMAP> Mosi<REMAP> for NoMosi {}
impl<REMAP> So<REMAP> for NoMiso {}
impl<REMAP> Si<REMAP> for NoMosi {}
 */

pub trait Instance:
    crate::Sealed + Deref<Target = crate::pac::spi1::RegisterBlock> + Enable + Reset + BusClock
{
    type MasterPins<INMODE>;
    type SlavePins<OUTMODE, INMODE>;
}

impl Instance for pac::SPI1 {
    type MasterPins<INMODE> = spi1::MasterPins<INMODE>;
    type SlavePins<OUTMODE, INMODE> = spi1::SlavePins<OUTMODE, INMODE>;
}
impl Instance for pac::SPI2 {
    type MasterPins<INMODE> = spi2::MasterPins<INMODE>;
    type SlavePins<OUTMODE, INMODE> = spi2::SlavePins<OUTMODE, INMODE>;
}
#[cfg(any(feature = "high", feature = "connectivity"))]
impl Instance for pac::SPI3 {
    type MasterPins<INMODE> = spi3::MasterPins<INMODE>;
    type SlavePins<OUTMODE, INMODE> = spi3::SlavePins<OUTMODE, INMODE>;
}

impl<SPI: Instance, INMODE> Spi<SPI, u8, INMODE> {
    /**
      Constructs an SPI instance using SPI1 in 8bit dataframe mode.

      The pin parameter tuple (sck, miso, mosi) should be `(PA5, PA6, PA7)` or `(PB3, PB4, PB5)` configured as `(Alternate<PushPull>, Input<...>, Alternate<PushPull>)`.

      You can also use `NoSck`, `NoMiso` or `NoMosi` if you don't want to use the pins
    */
    pub fn new(
        spi: SPI,
        pins: impl Into<SPI::MasterPins<INMODE>>,
        mode: Mode,
        freq: Hertz,
        clocks: &Clocks,
    ) -> Self {
        // enable or reset SPI
        let rcc = unsafe { &(*RCC::ptr()) };
        SPI::enable(rcc);
        SPI::reset(rcc);

        // disable SS output
        spi.cr2.write(|w| w.ssoe().clear_bit());

        let br = match SPI::clock(clocks) / freq {
            0 => unreachable!(),
            1..=2 => 0b000,
            3..=5 => 0b001,
            6..=11 => 0b010,
            12..=23 => 0b011,
            24..=47 => 0b100,
            48..=95 => 0b101,
            96..=191 => 0b110,
            _ => 0b111,
        };

        let pins = pins.into();

        spi.cr1.write(|w| {
            w
                // clock phase from config
                .cpha()
                .bit(mode.phase == Phase::CaptureOnSecondTransition)
                // clock polarity from config
                .cpol()
                .bit(mode.polarity == Polarity::IdleHigh)
                // mstr: master configuration
                .mstr()
                .set_bit()
                // baudrate value
                .br()
                .bits(br)
                // lsbfirst: MSB first
                .lsbfirst()
                .clear_bit()
                // ssm: enable software slave management (NSS pin free for other uses)
                .ssm()
                .set_bit()
                // ssi: set nss high = master mode
                .ssi()
                .set_bit()
                // dff: 8 bit frames
                .dff()
                .clear_bit()
                // bidimode: 2-line unidirectional
                .bidimode()
                .clear_bit()
                // both TX and RX are used
                .rxonly()
                .clear_bit()
                // spe: enable the SPI bus
                .spe()
                .set_bit()
        });

        Spi {
            inner: SpiInner::new(spi),
            pins,
        }
    }

    pub fn release(self) -> (SPI, SPI::MasterPins<INMODE>) {
        (self.inner.spi, self.pins)
    }
}

impl<SPI: Instance, OUTMODE, INMODE> SpiSlave<SPI, u8, OUTMODE, INMODE> {
    /**
      Constructs an SPI instance using SPI1 in 8bit dataframe mode.

      The pin parameter tuple (sck, miso, mosi) should be `(PA5, PA6, PA7)` or `(PB3, PB4, PB5)` configured as `(Input<Floating>, Alternate<...>, Input<...>)`.

      You can also use `NoMiso` or `NoMosi` if you don't want to use the pins
    */
    pub fn new(spi: SPI, pins: impl Into<SPI::SlavePins<OUTMODE, INMODE>>, mode: Mode) -> Self {
        // enable or reset SPI
        let rcc = unsafe { &(*RCC::ptr()) };
        SPI::enable(rcc);
        SPI::reset(rcc);

        // disable SS output
        spi.cr2.write(|w| w.ssoe().clear_bit());

        let pins = pins.into();

        spi.cr1.write(|w| {
            w
                // clock phase from config
                .cpha()
                .bit(mode.phase == Phase::CaptureOnSecondTransition)
                // clock polarity from config
                .cpol()
                .bit(mode.polarity == Polarity::IdleHigh)
                // mstr: slave configuration
                .mstr()
                .clear_bit()
                // lsbfirst: MSB first
                .lsbfirst()
                .clear_bit()
                // ssm: enable software slave management (NSS pin free for other uses)
                .ssm()
                .set_bit()
                // ssi: set nss low = slave mode
                .ssi()
                .clear_bit()
                // dff: 8 bit frames
                .dff()
                .clear_bit()
                // bidimode: 2-line unidirectional
                .bidimode()
                .clear_bit()
                // both TX and RX are used
                .rxonly()
                .clear_bit()
                // spe: enable the SPI bus
                .spe()
                .set_bit()
        });

        SpiSlave {
            inner: SpiInner::new(spi),
            pins,
        }
    }

    pub fn release(self) -> (SPI, SPI::SlavePins<OUTMODE, INMODE>) {
        (self.inner.spi, self.pins)
    }
}

pub trait SpiReadWrite<T> {
    fn read_data_reg(&mut self) -> T;
    fn write_data_reg(&mut self, data: T);
    fn spi_write(&mut self, words: &[T]) -> Result<(), Error>;
}

impl<SPI: Instance, FrameSize: Copy> SpiReadWrite<FrameSize> for SpiInner<SPI, FrameSize> {
    fn read_data_reg(&mut self) -> FrameSize {
        // NOTE(read_volatile) read only 1 byte (the svd2rust API only allows
        // reading a half-word)
        unsafe { ptr::read_volatile(&self.spi.dr as *const _ as *const FrameSize) }
    }

    fn write_data_reg(&mut self, data: FrameSize) {
        // NOTE(write_volatile) see note above
        unsafe { ptr::write_volatile(&self.spi.dr as *const _ as *mut FrameSize, data) }
    }

    // Implement write as per the "Transmit only procedure" page 712
    // of RM0008 Rev 20. This is more than twice as fast as the
    // default Write<> implementation (which reads and drops each
    // received value)
    fn spi_write(&mut self, words: &[FrameSize]) -> Result<(), Error> {
        // Write each word when the tx buffer is empty
        for word in words {
            loop {
                let sr = self.spi.sr.read();
                if sr.txe().bit_is_set() {
                    // NOTE(write_volatile) see note above
                    // unsafe { ptr::write_volatile(&self.spi.dr as *const _ as *mut u8, *word) }
                    self.write_data_reg(*word);
                    if sr.modf().bit_is_set() {
                        return Err(Error::ModeFault);
                    }
                    break;
                }
            }
        }
        // Wait for final TXE
        loop {
            let sr = self.spi.sr.read();
            if sr.txe().bit_is_set() {
                break;
            }
        }
        // Wait for final !BSY
        loop {
            let sr = self.spi.sr.read();
            if !sr.bsy().bit_is_set() {
                break;
            }
        }
        // Clear OVR set due to dropped received values
        // NOTE(read_volatile) see note above
        // unsafe {
        //     let _ = ptr::read_volatile(&self.spi.dr as *const _ as *const u8);
        // }
        let _ = self.read_data_reg();
        let _ = self.spi.sr.read();
        Ok(())
    }
}

impl<SPI: Instance, FrameSize: Copy> SpiInner<SPI, FrameSize> {
    /// Select which frame format is used for data transfers
    pub fn bit_format(&mut self, format: SpiBitFormat) {
        match format {
            SpiBitFormat::LsbFirst => self.spi.cr1.modify(|_, w| w.lsbfirst().set_bit()),
            SpiBitFormat::MsbFirst => self.spi.cr1.modify(|_, w| w.lsbfirst().clear_bit()),
        }
    }

    /// Starts listening to the SPI by enabling the _Received data
    /// ready to be read (RXNE)_ interrupt and _Transmit data
    /// register empty (TXE)_ interrupt
    pub fn listen(&mut self, event: Event) {
        match event {
            Event::Rxne => self.spi.cr2.modify(|_, w| w.rxneie().set_bit()),
            Event::Txe => self.spi.cr2.modify(|_, w| w.txeie().set_bit()),
            Event::Error => self.spi.cr2.modify(|_, w| w.errie().set_bit()),
        }
    }

    /// Stops listening to the SPI by disabling the _Received data
    /// ready to be read (RXNE)_ interrupt and _Transmit data
    /// register empty (TXE)_ interrupt
    pub fn unlisten(&mut self, event: Event) {
        match event {
            Event::Rxne => self.spi.cr2.modify(|_, w| w.rxneie().clear_bit()),
            Event::Txe => self.spi.cr2.modify(|_, w| w.txeie().clear_bit()),
            Event::Error => self.spi.cr2.modify(|_, w| w.errie().clear_bit()),
        }
    }

    /// Returns true if the tx register is empty (and can accept data)
    pub fn is_tx_empty(&self) -> bool {
        self.spi.sr.read().txe().bit_is_set()
    }

    /// Returns true if the rx register is not empty (and can be read)
    pub fn is_rx_not_empty(&self) -> bool {
        self.spi.sr.read().rxne().bit_is_set()
    }

    /// Returns true if data are received and the previous data have not yet been read from SPI_DR.
    pub fn is_overrun(&self) -> bool {
        self.spi.sr.read().ovr().bit_is_set()
    }

    pub fn is_busy(&self) -> bool {
        self.spi.sr.read().bsy().bit_is_set()
    }
}

impl<SPI: Instance, INMODE> Spi<SPI, u8, INMODE> {
    /// Converts from 8bit dataframe to 16bit.
    pub fn frame_size_16bit(self) -> Spi<SPI, u16, INMODE> {
        self.spi.cr1.modify(|_, w| w.spe().clear_bit());
        self.spi.cr1.modify(|_, w| w.dff().set_bit());
        self.spi.cr1.modify(|_, w| w.spe().set_bit());
        Spi {
            inner: SpiInner::new(self.inner.spi),
            pins: self.pins,
        }
    }
}

impl<SPI: Instance, OUTMODE, INMODE> SpiSlave<SPI, u8, OUTMODE, INMODE> {
    /// Converts from 8bit dataframe to 16bit.
    pub fn frame_size_16bit(self) -> SpiSlave<SPI, u16, OUTMODE, INMODE> {
        self.spi.cr1.modify(|_, w| w.spe().clear_bit());
        self.spi.cr1.modify(|_, w| w.dff().set_bit());
        self.spi.cr1.modify(|_, w| w.spe().set_bit());
        SpiSlave {
            inner: SpiInner::new(self.inner.spi),
            pins: self.pins,
        }
    }
}

impl<SPI: Instance, INMODE> Spi<SPI, u16, INMODE> {
    /// Converts from 16bit dataframe to 8bit.
    pub fn frame_size_8bit(self) -> Spi<SPI, u16, INMODE> {
        self.spi.cr1.modify(|_, w| w.spe().clear_bit());
        self.spi.cr1.modify(|_, w| w.dff().clear_bit());
        self.spi.cr1.modify(|_, w| w.spe().set_bit());
        Spi {
            inner: SpiInner::new(self.inner.spi),
            pins: self.pins,
        }
    }
}

impl<SPI: Instance, OUTMODE, INMODE> SpiSlave<SPI, u16, OUTMODE, INMODE> {
    /// Converts from 16bit dataframe to 8bit.
    pub fn frame_size_8bit(self) -> SpiSlave<SPI, u8, OUTMODE, INMODE> {
        self.spi.cr1.modify(|_, w| w.spe().clear_bit());
        self.spi.cr1.modify(|_, w| w.dff().clear_bit());
        self.spi.cr1.modify(|_, w| w.spe().set_bit());
        SpiSlave {
            inner: SpiInner::new(self.inner.spi),
            pins: self.pins,
        }
    }
}

impl<SPI: Instance, FrameSize: Copy> crate::hal::spi::FullDuplex<FrameSize>
    for SpiInner<SPI, FrameSize>
{
    type Error = Error;

    fn read(&mut self) -> nb::Result<FrameSize, Error> {
        let sr = self.spi.sr.read();

        Err(if sr.ovr().bit_is_set() {
            nb::Error::Other(Error::Overrun)
        } else if sr.modf().bit_is_set() {
            nb::Error::Other(Error::ModeFault)
        } else if sr.crcerr().bit_is_set() {
            nb::Error::Other(Error::Crc)
        } else if sr.rxne().bit_is_set() {
            // NOTE(read_volatile) read only 1 byte (the svd2rust API only allows
            // reading a half-word)
            return Ok(self.read_data_reg());
        } else {
            nb::Error::WouldBlock
        })
    }

    fn send(&mut self, data: FrameSize) -> nb::Result<(), Error> {
        let sr = self.spi.sr.read();

        Err(if sr.modf().bit_is_set() {
            nb::Error::Other(Error::ModeFault)
        } else if sr.crcerr().bit_is_set() {
            nb::Error::Other(Error::Crc)
        } else if sr.txe().bit_is_set() {
            // NOTE(write_volatile) see note above
            self.write_data_reg(data);
            return Ok(());
        } else {
            nb::Error::WouldBlock
        })
    }
}

impl<SPI: Instance, FrameSize: Copy> crate::hal::blocking::spi::transfer::Default<FrameSize>
    for SpiInner<SPI, FrameSize>
{
}

impl<SPI: Instance> crate::hal::blocking::spi::Write<u8> for SpiInner<SPI, u8> {
    type Error = Error;

    // Implement write as per the "Transmit only procedure" page 712
    // of RM0008 Rev 20. This is more than twice as fast as the
    // default Write<> implementation (which reads and drops each
    // received value)
    fn write(&mut self, words: &[u8]) -> Result<(), Error> {
        self.spi_write(words)
    }
}

impl<SPI: Instance> crate::hal::blocking::spi::Write<u16> for SpiInner<SPI, u16> {
    type Error = Error;

    fn write(&mut self, words: &[u16]) -> Result<(), Error> {
        self.spi_write(words)
    }
}

// DMA

pub type SpiTxDma<SPI, CHANNEL, INMODE = Floating> = TxDma<Spi<SPI, u8, INMODE>, CHANNEL>;
pub type SpiRxDma<SPI, CHANNEL, INMODE = Floating> = RxDma<Spi<SPI, u8, INMODE>, CHANNEL>;
pub type SpiRxTxDma<SPI, RXCHANNEL, TXCHANNEL, INMODE = Floating> =
    RxTxDma<Spi<SPI, u8, INMODE>, RXCHANNEL, TXCHANNEL>;

pub type SpiSlaveTxDma<SPI, CHANNEL, OUTMODE, INMODE = Floating> =
    TxDma<SpiSlave<SPI, u8, OUTMODE, INMODE>, CHANNEL>;
pub type SpiSlaveRxDma<SPI, CHANNEL, OUTMODE, INMODE = Floating> =
    RxDma<SpiSlave<SPI, u8, OUTMODE, INMODE>, CHANNEL>;
pub type SpiSlaveRxTxDma<SPI, RXCHANNEL, TXCHANNEL, OUTMODE, INMODE = Floating> =
    RxTxDma<SpiSlave<SPI, u8, OUTMODE, INMODE>, RXCHANNEL, TXCHANNEL>;

macro_rules! spi_dma {
    ($SPIi:ty, $RCi:ty, $TCi:ty, $rxdma:ident, $txdma:ident, $rxtxdma:ident, $slaverxdma:ident, $slavetxdma:ident, $slaverxtxdma:ident) => {
        pub type $rxdma<INMODE = Floating> = SpiRxDma<$SPIi, $RCi, INMODE>;
        pub type $txdma<INMODE = Floating> = SpiTxDma<$SPIi, $TCi, INMODE>;
        pub type $rxtxdma<INMODE = Floating> = SpiRxTxDma<$SPIi, $RCi, $TCi, INMODE>;

        impl<INMODE> Transmit for SpiTxDma<$SPIi, $TCi, INMODE> {
            type TxChannel = $TCi;
            type ReceivedWord = u8;
        }

        impl<INMODE> Receive for SpiRxDma<$SPIi, $RCi, INMODE> {
            type RxChannel = $RCi;
            type TransmittedWord = u8;
        }

        impl<INMODE> Transmit for SpiRxTxDma<$SPIi, $RCi, $TCi, INMODE> {
            type TxChannel = $TCi;
            type ReceivedWord = u8;
        }

        impl<INMODE> Receive for SpiRxTxDma<$SPIi, $RCi, $TCi, INMODE> {
            type RxChannel = $RCi;
            type TransmittedWord = u8;
        }

        impl<INMODE> Spi<$SPIi, u8, INMODE> {
            pub fn with_tx_dma(self, channel: $TCi) -> SpiTxDma<$SPIi, $TCi, INMODE> {
                self.spi.cr2.modify(|_, w| w.txdmaen().set_bit());
                SpiTxDma {
                    payload: self,
                    channel,
                }
            }
            pub fn with_rx_dma(self, channel: $RCi) -> SpiRxDma<$SPIi, $RCi, INMODE> {
                self.spi.cr2.modify(|_, w| w.rxdmaen().set_bit());
                SpiRxDma {
                    payload: self,
                    channel,
                }
            }
            pub fn with_rx_tx_dma(
                self,
                rxchannel: $RCi,
                txchannel: $TCi,
            ) -> SpiRxTxDma<$SPIi, $RCi, $TCi, INMODE> {
                self.spi
                    .cr2
                    .modify(|_, w| w.rxdmaen().set_bit().txdmaen().set_bit());
                SpiRxTxDma {
                    payload: self,
                    rxchannel,
                    txchannel,
                }
            }
        }

        impl<INMODE> SpiTxDma<$SPIi, $TCi, INMODE> {
            pub fn release(self) -> (Spi<$SPIi, u8, INMODE>, $TCi) {
                let SpiTxDma { payload, channel } = self;
                payload.spi.cr2.modify(|_, w| w.txdmaen().clear_bit());
                (payload, channel)
            }
        }

        impl<INMODE> SpiRxDma<$SPIi, $RCi, INMODE> {
            pub fn release(self) -> (Spi<$SPIi, u8, INMODE>, $RCi) {
                let SpiRxDma { payload, channel } = self;
                payload.spi.cr2.modify(|_, w| w.rxdmaen().clear_bit());
                (payload, channel)
            }
        }

        impl<INMODE> SpiRxTxDma<$SPIi, $RCi, $TCi, INMODE> {
            pub fn release(self) -> (Spi<$SPIi, u8, INMODE>, $RCi, $TCi) {
                let SpiRxTxDma {
                    payload,
                    rxchannel,
                    txchannel,
                } = self;
                payload
                    .spi
                    .cr2
                    .modify(|_, w| w.rxdmaen().clear_bit().txdmaen().clear_bit());
                (payload, rxchannel, txchannel)
            }
        }

        impl<INMODE> TransferPayload for SpiTxDma<$SPIi, $TCi, INMODE> {
            fn start(&mut self) {
                self.channel.start();
            }
            fn stop(&mut self) {
                self.channel.stop();
            }
        }

        impl<INMODE> TransferPayload for SpiRxDma<$SPIi, $RCi, INMODE> {
            fn start(&mut self) {
                self.channel.start();
            }
            fn stop(&mut self) {
                self.channel.stop();
            }
        }

        impl<INMODE> TransferPayload for SpiRxTxDma<$SPIi, $RCi, $TCi, INMODE> {
            fn start(&mut self) {
                self.rxchannel.start();
                self.txchannel.start();
            }
            fn stop(&mut self) {
                self.txchannel.stop();
                self.rxchannel.stop();
            }
        }

        impl<B, INMODE> crate::dma::ReadDma<B, u8> for SpiRxDma<$SPIi, $RCi, INMODE>
        where
            B: WriteBuffer<Word = u8>,
        {
            fn read(mut self, mut buffer: B) -> Transfer<W, B, Self> {
                // NOTE(unsafe) We own the buffer now and we won't call other `&mut` on it
                // until the end of the transfer.
                let (ptr, len) = unsafe { buffer.write_buffer() };
                self.channel.set_peripheral_address(
                    unsafe { &(*<$SPIi>::ptr()).dr as *const _ as u32 },
                    false,
                );
                self.channel.set_memory_address(ptr as u32, true);
                self.channel.set_transfer_length(len);

                atomic::compiler_fence(Ordering::Release);
                self.channel.ch().cr.modify(|_, w| {
                    w
                        // memory to memory mode disabled
                        .mem2mem()
                        .clear_bit()
                        // medium channel priority level
                        .pl()
                        .medium()
                        // 8-bit memory size
                        .msize()
                        .bits8()
                        // 8-bit peripheral size
                        .psize()
                        .bits8()
                        // circular mode disabled
                        .circ()
                        .clear_bit()
                        // write to memory
                        .dir()
                        .clear_bit()
                });
                self.start();

                Transfer::w(buffer, self)
            }
        }

        impl<B, INMODE> crate::dma::WriteDma<B, u8> for SpiTxDma<$SPIi, $TCi, INMODE>
        where
            B: ReadBuffer<Word = u8>,
        {
            fn write(mut self, buffer: B) -> Transfer<R, B, Self> {
                // NOTE(unsafe) We own the buffer now and we won't call other `&mut` on it
                // until the end of the transfer.
                let (ptr, len) = unsafe { buffer.read_buffer() };
                self.channel.set_peripheral_address(
                    unsafe { &(*<$SPIi>::ptr()).dr as *const _ as u32 },
                    false,
                );
                self.channel.set_memory_address(ptr as u32, true);
                self.channel.set_transfer_length(len);

                atomic::compiler_fence(Ordering::Release);
                self.channel.ch().cr.modify(|_, w| {
                    w
                        // memory to memory mode disabled
                        .mem2mem()
                        .clear_bit()
                        // medium channel priority level
                        .pl()
                        .medium()
                        // 8-bit memory size
                        .msize()
                        .bits8()
                        // 8-bit peripheral size
                        .psize()
                        .bits8()
                        // circular mode disabled
                        .circ()
                        .clear_bit()
                        // read from memory
                        .dir()
                        .set_bit()
                });
                self.start();

                Transfer::r(buffer, self)
            }
        }

        impl<RXB, TXB, INMODE> crate::dma::ReadWriteDma<RXB, TXB, u8>
            for SpiRxTxDma<$SPIi, $RCi, $TCi, INMODE>
        where
            RXB: WriteBuffer<Word = u8>,
            TXB: ReadBuffer<Word = u8>,
        {
            fn read_write(
                mut self,
                mut rxbuffer: RXB,
                txbuffer: TXB,
            ) -> Transfer<W, (RXB, TXB), Self> {
                // NOTE(unsafe) We own the buffer now and we won't call other `&mut` on it
                // until the end of the transfer.
                let (rxptr, rxlen) = unsafe { rxbuffer.write_buffer() };
                let (txptr, txlen) = unsafe { txbuffer.read_buffer() };

                if rxlen != txlen {
                    panic!("receive and send buffer lengths do not match!");
                }

                self.rxchannel.set_peripheral_address(
                    unsafe { &(*<$SPIi>::ptr()).dr as *const _ as u32 },
                    false,
                );
                self.rxchannel.set_memory_address(rxptr as u32, true);
                self.rxchannel.set_transfer_length(rxlen);

                self.txchannel.set_peripheral_address(
                    unsafe { &(*<$SPIi>::ptr()).dr as *const _ as u32 },
                    false,
                );
                self.txchannel.set_memory_address(txptr as u32, true);
                self.txchannel.set_transfer_length(txlen);

                atomic::compiler_fence(Ordering::Release);
                self.rxchannel.ch().cr.modify(|_, w| {
                    w
                        // memory to memory mode disabled
                        .mem2mem()
                        .clear_bit()
                        // medium channel priority level
                        .pl()
                        .medium()
                        // 8-bit memory size
                        .msize()
                        .bits8()
                        // 8-bit peripheral size
                        .psize()
                        .bits8()
                        // circular mode disabled
                        .circ()
                        .clear_bit()
                        // write to memory
                        .dir()
                        .clear_bit()
                });
                self.txchannel.ch().cr.modify(|_, w| {
                    w
                        // memory to memory mode disabled
                        .mem2mem()
                        .clear_bit()
                        // medium channel priority level
                        .pl()
                        .medium()
                        // 8-bit memory size
                        .msize()
                        .bits8()
                        // 8-bit peripheral size
                        .psize()
                        .bits8()
                        // circular mode disabled
                        .circ()
                        .clear_bit()
                        // read from memory
                        .dir()
                        .set_bit()
                });
                self.start();

                Transfer::w((rxbuffer, txbuffer), self)
            }
        }

        pub type $slaverxdma<OUTMODE = PushPull, INMODE = Floating> =
            SpiSlaveRxDma<$SPIi, $RCi, OUTMODE, INMODE>;
        pub type $slavetxdma<OUTMODE = PushPull, INMODE = Floating> =
            SpiSlaveTxDma<$SPIi, $TCi, OUTMODE, INMODE>;
        pub type $slaverxtxdma<OUTMODE = PushPull, INMODE = Floating> =
            SpiSlaveRxTxDma<$SPIi, $RCi, $TCi, OUTMODE, INMODE>;

        impl<OUTMODE, INMODE> Transmit for SpiSlaveTxDma<$SPIi, $TCi, OUTMODE, INMODE> {
            type TxChannel = $TCi;
            type ReceivedWord = u8;
        }

        impl<OUTMODE, INMODE> Receive for SpiSlaveRxDma<$SPIi, $RCi, OUTMODE, INMODE> {
            type RxChannel = $RCi;
            type TransmittedWord = u8;
        }

        impl<OUTMODE, INMODE> Transmit for SpiSlaveRxTxDma<$SPIi, $RCi, $TCi, OUTMODE, INMODE> {
            type TxChannel = $TCi;
            type ReceivedWord = u8;
        }

        impl<OUTMODE, INMODE> Receive for SpiSlaveRxTxDma<$SPIi, $RCi, $TCi, OUTMODE, INMODE> {
            type RxChannel = $RCi;
            type TransmittedWord = u8;
        }

        impl<OUTMODE, INMODE> SpiSlave<$SPIi, u8, OUTMODE, INMODE> {
            pub fn with_tx_dma(self, channel: $TCi) -> SpiSlaveTxDma<$SPIi, $TCi, OUTMODE, INMODE> {
                self.spi.cr2.modify(|_, w| w.txdmaen().set_bit());
                SpiSlaveTxDma {
                    payload: self,
                    channel,
                }
            }
            pub fn with_rx_dma(self, channel: $RCi) -> SpiSlaveRxDma<$SPIi, $RCi, OUTMODE, INMODE> {
                self.spi.cr2.modify(|_, w| w.rxdmaen().set_bit());
                SpiSlaveRxDma {
                    payload: self,
                    channel,
                }
            }
            pub fn with_rx_tx_dma(
                self,
                rxchannel: $RCi,
                txchannel: $TCi,
            ) -> SpiSlaveRxTxDma<$SPIi, $RCi, $TCi, OUTMODE, INMODE> {
                self.spi
                    .cr2
                    .modify(|_, w| w.rxdmaen().set_bit().txdmaen().set_bit());
                SpiSlaveRxTxDma {
                    payload: self,
                    rxchannel,
                    txchannel,
                }
            }
        }

        impl<OUTMODE, INMODE> SpiSlaveTxDma<$SPIi, $TCi, OUTMODE, INMODE> {
            pub fn release(self) -> (SpiSlave<$SPIi, u8, OUTMODE, INMODE>, $TCi) {
                let SpiSlaveTxDma { payload, channel } = self;
                payload.spi.cr2.modify(|_, w| w.txdmaen().clear_bit());
                (payload, channel)
            }
        }

        impl<OUTMODE, INMODE> SpiSlaveRxDma<$SPIi, $RCi, OUTMODE, INMODE> {
            pub fn release(self) -> (SpiSlave<$SPIi, u8, OUTMODE, INMODE>, $RCi) {
                let SpiSlaveRxDma { payload, channel } = self;
                payload.spi.cr2.modify(|_, w| w.rxdmaen().clear_bit());
                (payload, channel)
            }
        }

        impl<OUTMODE, INMODE> SpiSlaveRxTxDma<$SPIi, $RCi, $TCi, OUTMODE, INMODE> {
            pub fn release(self) -> (SpiSlave<$SPIi, u8, OUTMODE, INMODE>, $RCi, $TCi) {
                let SpiSlaveRxTxDma {
                    payload,
                    rxchannel,
                    txchannel,
                } = self;
                payload
                    .spi
                    .cr2
                    .modify(|_, w| w.rxdmaen().clear_bit().txdmaen().clear_bit());
                (payload, rxchannel, txchannel)
            }
        }

        impl<OUTMODE, INMODE> TransferPayload for SpiSlaveTxDma<$SPIi, $TCi, OUTMODE, INMODE> {
            fn start(&mut self) {
                self.channel.start();
            }
            fn stop(&mut self) {
                self.channel.stop();
            }
        }

        impl<OUTMODE, INMODE> TransferPayload for SpiSlaveRxDma<$SPIi, $RCi, OUTMODE, INMODE> {
            fn start(&mut self) {
                self.channel.start();
            }
            fn stop(&mut self) {
                self.channel.stop();
            }
        }

        impl<OUTMODE, INMODE> TransferPayload
            for SpiSlaveRxTxDma<$SPIi, $RCi, $TCi, OUTMODE, INMODE>
        {
            fn start(&mut self) {
                self.rxchannel.start();
                self.txchannel.start();
            }
            fn stop(&mut self) {
                self.txchannel.stop();
                self.rxchannel.stop();
            }
        }

        impl<B, OUTMODE, INMODE> crate::dma::ReadDma<B, u8>
            for SpiSlaveRxDma<$SPIi, $RCi, OUTMODE, INMODE>
        where
            B: WriteBuffer<Word = u8>,
        {
            fn read(mut self, mut buffer: B) -> Transfer<W, B, Self> {
                // NOTE(unsafe) We own the buffer now and we won't call other `&mut` on it
                // until the end of the transfer.
                let (ptr, len) = unsafe { buffer.write_buffer() };
                self.channel.set_peripheral_address(
                    unsafe { &(*<$SPIi>::ptr()).dr as *const _ as u32 },
                    false,
                );
                self.channel.set_memory_address(ptr as u32, true);
                self.channel.set_transfer_length(len);

                atomic::compiler_fence(Ordering::Release);
                self.channel.ch().cr.modify(|_, w| {
                    w
                        // memory to memory mode disabled
                        .mem2mem()
                        .clear_bit()
                        // medium channel priority level
                        .pl()
                        .medium()
                        // 8-bit memory size
                        .msize()
                        .bits8()
                        // 8-bit peripheral size
                        .psize()
                        .bits8()
                        // circular mode disabled
                        .circ()
                        .clear_bit()
                        // write to memory
                        .dir()
                        .clear_bit()
                });
                self.start();

                Transfer::w(buffer, self)
            }
        }

        impl<B, OUTMODE, INMODE> crate::dma::WriteDma<B, u8>
            for SpiSlaveTxDma<$SPIi, $TCi, OUTMODE, INMODE>
        where
            B: ReadBuffer<Word = u8>,
        {
            fn write(mut self, buffer: B) -> Transfer<R, B, Self> {
                // NOTE(unsafe) We own the buffer now and we won't call other `&mut` on it
                // until the end of the transfer.
                let (ptr, len) = unsafe { buffer.read_buffer() };
                self.channel.set_peripheral_address(
                    unsafe { &(*<$SPIi>::ptr()).dr as *const _ as u32 },
                    false,
                );
                self.channel.set_memory_address(ptr as u32, true);
                self.channel.set_transfer_length(len);

                atomic::compiler_fence(Ordering::Release);
                self.channel.ch().cr.modify(|_, w| {
                    w
                        // memory to memory mode disabled
                        .mem2mem()
                        .clear_bit()
                        // medium channel priority level
                        .pl()
                        .medium()
                        // 8-bit memory size
                        .msize()
                        .bits8()
                        // 8-bit peripheral size
                        .psize()
                        .bits8()
                        // circular mode disabled
                        .circ()
                        .clear_bit()
                        // read from memory
                        .dir()
                        .set_bit()
                });
                self.start();

                Transfer::r(buffer, self)
            }
        }

        impl<RXB, TXB, OUTMODE, INMODE> crate::dma::ReadWriteDma<RXB, TXB, u8>
            for SpiSlaveRxTxDma<$SPIi, $RCi, $TCi, OUTMODE, INMODE>
        where
            RXB: WriteBuffer<Word = u8>,
            TXB: ReadBuffer<Word = u8>,
        {
            fn read_write(
                mut self,
                mut rxbuffer: RXB,
                txbuffer: TXB,
            ) -> Transfer<W, (RXB, TXB), Self> {
                // NOTE(unsafe) We own the buffer now and we won't call other `&mut` on it
                // until the end of the transfer.
                let (rxptr, rxlen) = unsafe { rxbuffer.write_buffer() };
                let (txptr, txlen) = unsafe { txbuffer.read_buffer() };

                if rxlen != txlen {
                    panic!("receive and send buffer lengths do not match!");
                }

                self.rxchannel.set_peripheral_address(
                    unsafe { &(*<$SPIi>::ptr()).dr as *const _ as u32 },
                    false,
                );
                self.rxchannel.set_memory_address(rxptr as u32, true);
                self.rxchannel.set_transfer_length(rxlen);

                self.txchannel.set_peripheral_address(
                    unsafe { &(*<$SPIi>::ptr()).dr as *const _ as u32 },
                    false,
                );
                self.txchannel.set_memory_address(txptr as u32, true);
                self.txchannel.set_transfer_length(txlen);

                atomic::compiler_fence(Ordering::Release);
                self.rxchannel.ch().cr.modify(|_, w| {
                    w
                        // memory to memory mode disabled
                        .mem2mem()
                        .clear_bit()
                        // medium channel priority level
                        .pl()
                        .medium()
                        // 8-bit memory size
                        .msize()
                        .bits8()
                        // 8-bit peripheral size
                        .psize()
                        .bits8()
                        // circular mode disabled
                        .circ()
                        .clear_bit()
                        // write to memory
                        .dir()
                        .clear_bit()
                });
                self.txchannel.ch().cr.modify(|_, w| {
                    w
                        // memory to memory mode disabled
                        .mem2mem()
                        .clear_bit()
                        // medium channel priority level
                        .pl()
                        .medium()
                        // 8-bit memory size
                        .msize()
                        .bits8()
                        // 8-bit peripheral size
                        .psize()
                        .bits8()
                        // circular mode disabled
                        .circ()
                        .clear_bit()
                        // read from memory
                        .dir()
                        .set_bit()
                });
                self.start();

                Transfer::w((rxbuffer, txbuffer), self)
            }
        }
    };
}

spi_dma!(
    pac::SPI1,
    dma1::C2,
    dma1::C3,
    Spi1RxDma,
    Spi1TxDma,
    Spi1RxTxDma,
    SpiSlave1RxDma,
    SpiSlave1TxDma,
    SpiSlave1RxTxDma
);
spi_dma!(
    pac::SPI2,
    dma1::C4,
    dma1::C5,
    Spi2RxDma,
    Spi2TxDma,
    Spi2RxTxDma,
    SpiSlave2RxDma,
    SpiSlave2TxDma,
    SpiSlave2RxTxDma
);
#[cfg(feature = "connectivity")]
spi_dma!(
    pac::SPI3,
    dma2::C1,
    dma2::C2,
    Spi3RxDma,
    Spi3TxDma,
    Spi3RxTxDma,
    SpiSlave3RxDma,
    SpiSlave3TxDma,
    SpiSlave3RxTxDma
);
