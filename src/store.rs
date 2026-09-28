//! Menu choices kept in flash, so they survive power-off: the chosen tags,
//! "Show picture names" and "Show clock". Until something is saved, the
//! frame starts with the settings from `settings.toml`.
//!
//! The choices live in the `nvs` partition, which nothing else uses (the
//! Wi-Fi driver keeps its NVS in RAM). Two sectors take turns: each save
//! erases the older one and writes a record with a sequence number and a
//! CRC, so if the power goes during a save the previous record is still
//! there. The choices change only when someone taps "Done", so the flash
//! won't wear out.
//!
//! Erasing a sector takes around 40 ms with interrupts off, which stops the
//! display refill for that long, so saves happen only when something has
//! changed, with the screen blanked (`frame::save_blanked`).

use alloc::string::String;
use alloc::vec::Vec;

use esp_bootloader_esp_idf::partitions::{self, DataPartitionSubType, PartitionType};
use esp_hal::peripherals::FLASH;
use esp_storage::FlashStorage;
use log::{info, warn};

/// What the menu chooses.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Choices {
    /// Empty means every picture.
    pub tags: Vec<String>,
    pub show_phrase: bool,
    pub show_clock: bool,
}

const SECTOR: u32 = FlashStorage::SECTOR_SIZE;
/// "CCD1": a record in this format.
const MAGIC: u32 = u32::from_le_bytes(*b"CCD1");
/// Magic, sequence number, payload length and CRC.
const HEADER: usize = 16;
/// Room for the record; tags that don't fit aren't saved.
const RECORD: usize = 1024;

const FLAG_SHOW_PHRASE: u8 = 1 << 0;
const FLAG_SHOW_CLOCK: u8 = 1 << 1;

/// A record, word-aligned as the flash routines need.
#[repr(C, align(4))]
struct Record([u8; RECORD]);

pub struct Store {
    flash: FlashStorage<'static>,
    /// Start of the `nvs` partition; `None` if there isn't one, and then
    /// nothing is saved.
    partition: Option<u32>,
    /// Sequence number of the newest record.
    sequence: u32,
    /// The sector (0 or 1) the next save goes to.
    next_slot: u32,
}

impl Store {
    pub fn new(flash: FLASH<'static>) -> Self {
        let mut flash = FlashStorage::new(flash);
        let mut table = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
        let partition = match partitions::read_partition_table(&mut flash, &mut table)
            .and_then(|t| t.find_partition(PartitionType::Data(DataPartitionSubType::Nvs)))
        {
            Ok(Some(p)) if p.len() >= 2 * SECTOR && p.offset() % SECTOR == 0 => Some(p.offset()),
            Ok(_) => {
                warn!("store: no nvs partition of 8 KB or more; menu choices won't be saved");
                None
            }
            Err(e) => {
                warn!("store: can't read the partition table ({e:?}); menu choices won't be saved");
                None
            }
        };
        Self { flash, partition, sequence: 0, next_slot: 0 }
    }

    /// The choices saved last, if there are any.
    pub fn load(&mut self) -> Option<Choices> {
        let partition = self.partition?;
        let mut newest: Option<(u32, u32, Choices)> = None;
        let mut record = Record([0; RECORD]);
        for slot in 0..2 {
            if self.flash.read_nor(partition + slot * SECTOR, &mut record.0).is_err() {
                continue;
            }
            if let Some((sequence, choices)) = decode(&record.0)
                && newest.as_ref().is_none_or(|(s, _, _)| sequence.wrapping_sub(*s) as i32 > 0)
            {
                newest = Some((sequence, slot, choices));
            }
        }
        let (sequence, slot, choices) = newest?;
        self.sequence = sequence;
        self.next_slot = 1 - slot;
        Some(choices)
    }

    /// Saves `choices`, replacing the older of the two records.
    pub fn save(&mut self, choices: &Choices) {
        let Some(partition) = self.partition else { return };
        let sequence = self.sequence.wrapping_add(1);
        let mut record = Record([0xFF; RECORD]);
        let length = encode(choices, sequence, &mut record.0);
        let address = partition + self.next_slot * SECTOR;
        let start = embassy_time::Instant::now();
        let result = self
            .flash
            .erase(address, address + SECTOR)
            .and_then(|_| self.flash.write_nor(address, &record.0[..length.next_multiple_of(4)]));
        match result {
            Ok(()) => {
                info!("store: saved {choices:?} in {} ms", start.elapsed().as_millis());
                self.sequence = sequence;
                self.next_slot = 1 - self.next_slot;
            }
            Err(e) => warn!("store: can't save: {e:?}"),
        }
    }
}

/// Writes a record for `choices` into `out`, and returns its length.
fn encode(choices: &Choices, sequence: u32, out: &mut [u8; RECORD]) -> usize {
    let mut flags = 0;
    if choices.show_phrase {
        flags |= FLAG_SHOW_PHRASE;
    }
    if choices.show_clock {
        flags |= FLAG_SHOW_CLOCK;
    }
    // Payload: flags, number of tags, then each tag as a length and its bytes.
    out[HEADER] = flags;
    let mut count = 0u8;
    let mut end = HEADER + 2;
    for tag in &choices.tags {
        let bytes = tag.as_bytes();
        if bytes.len() > u8::MAX as usize || end + 1 + bytes.len() > RECORD || count == u8::MAX {
            warn!("store: no room to save tag {tag:?}");
            continue;
        }
        out[end] = bytes.len() as u8;
        out[end + 1..end + 1 + bytes.len()].copy_from_slice(bytes);
        end += 1 + bytes.len();
        count += 1;
    }
    out[HEADER + 1] = count;

    let length = (end - HEADER) as u32;
    out[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    out[4..8].copy_from_slice(&sequence.to_le_bytes());
    out[8..12].copy_from_slice(&length.to_le_bytes());
    let crc = crc32(&[&out[4..12], &out[HEADER..end]]);
    out[12..16].copy_from_slice(&crc.to_le_bytes());
    end
}

/// Reads a record: its sequence number and the choices, or `None` if the
/// sector is erased or the record is damaged.
fn decode(record: &[u8; RECORD]) -> Option<(u32, Choices)> {
    let word = |i: usize| u32::from_le_bytes([record[i], record[i + 1], record[i + 2], record[i + 3]]);
    if word(0) != MAGIC {
        return None;
    }
    let (sequence, length, crc) = (word(4), word(8) as usize, word(12));
    if length < 2 || HEADER + length > RECORD {
        return None;
    }
    let payload = &record[HEADER..HEADER + length];
    if crc32(&[&record[4..12], payload]) != crc {
        return None;
    }

    let flags = payload[0];
    let mut tags = Vec::new();
    let mut rest = &payload[2..];
    for _ in 0..payload[1] {
        let (&len, after) = rest.split_first()?;
        let bytes = after.get(..len as usize)?;
        tags.push(String::from(core::str::from_utf8(bytes).ok()?));
        rest = &after[len as usize..];
    }
    Some((
        sequence,
        Choices {
            tags,
            show_phrase: flags & FLAG_SHOW_PHRASE != 0,
            show_clock: flags & FLAG_SHOW_CLOCK != 0,
        },
    ))
}

/// CRC-32 (IEEE) of the parts, one after another.
fn crc32(parts: &[&[u8]]) -> u32 {
    let mut crc = !0u32;
    for byte in parts.iter().flat_map(|p| p.iter()) {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}
