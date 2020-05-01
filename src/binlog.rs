// Copyright (c) 2020 Anatoly Ikorsky
//
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or http://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. All files in the project carrying such notice may not be copied,
// modified, or distributed except according to those terms.

//! Binlog-related structures and functions. This implementation assumes
//! binlog version >= 4 (MySql >= 5.0.0).
//!
//! All structures of this module contains raw data that may not necessarily be valid.
//! Please consult the MySql documentation.

use bitvec::{order::Lsb0, vec::BitVec};
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use num_traits::{Bounded, PrimInt};
use saturating::Saturating as S;

use std::{
    borrow::Cow,
    cmp::min,
    convert::TryFrom,
    fmt,
    hash::{Hash, Hasher},
    io::{
        self, Error,
        ErrorKind::{InvalidData, Other, UnexpectedEof},
        Read, Write,
    },
    marker::PhantomData,
};

use crate::{
    constants::{ColumnType, ItemResult, UnknownColumnType, UnknownItemResultType},
    io::{ReadMysqlExt, WriteMysqlExt},
    misc::{LimitRead, LimitWrite},
    Bitflags,
};

/// Wrapper for a raw value of a particular type.
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[repr(transparent)]
pub struct RawField<T, E, V>(pub T, PhantomData<(E, V)>);

impl<T: Copy, U: Into<T>, V: TryFrom<T, Error = U>> RawField<T, U, V> {
    /// Creates a new wrapper.
    pub fn new(t: T) -> Self {
        Self(t, PhantomData)
    }

    /// Returns either parsed value of this field, or raw value in case of an error.
    pub fn get(&self) -> Result<V, U> {
        V::try_from(self.0)
    }
}

impl<T: fmt::Debug, U: fmt::Debug, V: fmt::Debug> fmt::Debug for RawField<T, U, V>
where
    T: Copy,
    U: Into<T>,
    V: TryFrom<T, Error = U>,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match V::try_from(self.0) {
            Ok(u) => u.fmt(f),
            Err(t) => write!(
                f,
                "Unknown value for type {}: {:?}",
                std::any::type_name::<U>(),
                t
            ),
        }
    }
}

/// Wrapper for a sequence of values of a particular type.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[repr(transparent)]
pub struct RawSeq<T, U, V>(pub Vec<T>, PhantomData<(U, V)>);

impl<T: Copy, U: Into<T>, V: TryFrom<T, Error = U>> RawSeq<T, U, V> {
    /// Creates a new wrapper.
    pub fn new(t: Vec<T>) -> Self {
        Self(t, PhantomData)
    }

    /// Returns either parsed value at the given index, or raw value in case of an error.
    pub fn get(&self, index: usize) -> Option<Result<V, U>> {
        self.0.get(index).copied().map(V::try_from)
    }

    /// Returns a length of this sequence.
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

impl<T: fmt::Debug, U: fmt::Debug, V: fmt::Debug> fmt::Debug for RawSeq<T, U, V>
where
    T: Copy,
    U: Into<T>,
    V: TryFrom<T, Error = U>,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0
            .iter()
            .copied()
            .map(RawField::<T, U, V>::new)
            .collect::<Vec<_>>()
            .fmt(f)
    }
}

/// Wrapper for raw flags value.
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct RawFlags<T: Bitflags>(pub T::Repr);

impl<T: Bitflags> RawFlags<T> {
    /// Returns parsed flags. Unknown bits will be truncated.
    pub fn get(&self) -> T {
        T::from_bits_truncate(self.0)
    }
}

impl<T: fmt::Debug> fmt::Debug for RawFlags<T>
where
    T: Bitflags,
    T::Repr: fmt::Binary,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.get())?;
        let unknown_bits = self.0 & (T::Repr::max_value() ^ T::all().bits());
        if unknown_bits.count_ones() > 0 {
            write!(
                f,
                " (Unknown bits: {:0width$b})",
                unknown_bits,
                width = T::Repr::max_value().count_ones() as usize,
            )?
        }
        Ok(())
    }
}

/// Wrapper for raw text value.
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct RawText<T = Vec<u8>>(pub T);

impl<T: AsRef<[u8]>> RawText<T> {
    /// Returns either parsed value of this field, or raw value in case of an error.
    pub fn get(&self) -> Cow<str> {
        let slice = self.0.as_ref();
        match slice.iter().position(|c| *c == 0) {
            Some(position) => String::from_utf8_lossy(&slice[..position]),
            None => String::from_utf8_lossy(slice),
        }
    }
}

impl<T: AsRef<[u8]>> fmt::Debug for RawText<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.get().fmt(f)
    }
}

/// Depending on the MySQL Version that created the binlog the format is slightly different.
#[repr(u8)]
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum BinlogVersion {
    /// MySQL 3.23 - < 4.0.0
    Version1 = 1,
    /// MySQL 4.0.0 - 4.0.1
    Version2,
    /// MySQL 4.0.2 - < 5.0.0
    Version3,
    /// MySQL 5.0.0+
    Version4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
#[error("Unknown binlog version {}", _0)]
#[repr(transparent)]
pub struct UnknownBinlogVersion(pub u16);

impl From<UnknownBinlogVersion> for u16 {
    fn from(x: UnknownBinlogVersion) -> Self {
        x.0
    }
}

impl TryFrom<u16> for BinlogVersion {
    type Error = UnknownBinlogVersion;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Version1),
            2 => Ok(Self::Version2),
            3 => Ok(Self::Version3),
            4 => Ok(Self::Version4),
            x => Err(UnknownBinlogVersion(x)),
        }
    }
}

/// Binlog Event Type
#[allow(non_camel_case_types)]
#[repr(u8)]
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum EventType {
    /// Ignored event.
    UNKNOWN_EVENT = 0x00,
    /// A start event is the first event of a binlog for binlog-version 1 to 3.
    ///
    /// Superseded by `FORMAT_DESCRIPTION_EVENT` since mysql v5.0.0.
    START_EVENT_V3 = 0x01,
    /// A `QUERY_EVENT` is created for each query that modifies the database,
    /// unless the query is logged row-based.
    QUERY_EVENT = 0x02,
    /// A `STOP_EVENT` has no payload or post-header.
    STOP_EVENT = 0x03,
    /// The rotate event is added to the binlog as last event
    /// to tell the reader what binlog to request next.
    ROTATE_EVENT = 0x04,
    INTVAR_EVENT = 0x05,
    LOAD_EVENT = 0x06,
    /// Ignored event.
    SLAVE_EVENT = 0x07,
    CREATE_FILE_EVENT = 0x08,
    APPEND_BLOCK_EVENT = 0x09,
    EXEC_LOAD_EVENT = 0x0a,
    DELETE_FILE_EVENT = 0x0b,
    NEW_LOAD_EVENT = 0x0c,
    RAND_EVENT = 0x0d,
    USER_VAR_EVENT = 0x0e,
    ///  A format description event is the first event of a binlog for binlog-version 4. It describes how the other events are layed out.
    ///
    /// # Note
    ///
    /// Added in MySQL 5.0.0 as replacement for START_EVENT_V3
    FORMAT_DESCRIPTION_EVENT = 0x0f,
    XID_EVENT = 0x10,
    BEGIN_LOAD_QUERY_EVENT = 0x11,
    EXECUTE_LOAD_QUERY_EVENT = 0x12,
    TABLE_MAP_EVENT = 0x13,
    PRE_GA_WRITE_ROWS_EVENT = 0x14,
    PRE_GA_UPDATE_ROWS_EVENT = 0x15,
    PRE_GA_DELETE_ROWS_EVENT = 0x16,
    WRITE_ROWS_EVENT_V1 = 0x17,
    UPDATE_ROWS_EVENT_V1 = 0x18,
    DELETE_ROWS_EVENT_V1 = 0x19,
    INCIDENT_EVENT = 0x1a,
    HEARTBEAT_EVENT = 0x1b,
    IGNORABLE_EVENT = 0x1c,
    ROWS_QUERY_EVENT = 0x1d,
    WRITE_ROWS_EVENT = 0x1e,
    UPDATE_ROWS_EVENT = 0x1f,
    DELETE_ROWS_EVENT = 0x20,
    GTID_EVENT = 0x21,
    ANONYMOUS_GTID_EVENT = 0x22,
    PREVIOUS_GTIDS_EVENT = 0x23,
    TRANSACTION_CONTEXT_EVENT = 0x24,
    VIEW_CHANGE_EVENT = 0x25,
    /// Prepared XA transaction terminal event similar to Xid.
    XA_PREPARE_LOG_EVENT = 0x26,
    /// Extension of UPDATE_ROWS_EVENT, allowing partial values according
    /// to binlog_row_value_options.
    PARTIAL_UPDATE_ROWS_EVENT = 0x27,
    /// Total number of known events.
    ENUM_END_EVENT,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
#[error("Unknown event type {}", _0)]
#[repr(transparent)]
pub struct UnknownEventType(pub u8);

impl From<UnknownEventType> for u8 {
    fn from(x: UnknownEventType) -> Self {
        x.0
    }
}

impl TryFrom<u8> for EventType {
    type Error = UnknownEventType;

    fn try_from(byte: u8) -> Result<Self, UnknownEventType> {
        match byte {
            0x00 => Ok(Self::UNKNOWN_EVENT),
            0x01 => Ok(Self::START_EVENT_V3),
            0x02 => Ok(Self::QUERY_EVENT),
            0x03 => Ok(Self::STOP_EVENT),
            0x04 => Ok(Self::ROTATE_EVENT),
            0x05 => Ok(Self::INTVAR_EVENT),
            0x06 => Ok(Self::LOAD_EVENT),
            0x07 => Ok(Self::SLAVE_EVENT),
            0x08 => Ok(Self::CREATE_FILE_EVENT),
            0x09 => Ok(Self::APPEND_BLOCK_EVENT),
            0x0a => Ok(Self::EXEC_LOAD_EVENT),
            0x0b => Ok(Self::DELETE_FILE_EVENT),
            0x0c => Ok(Self::NEW_LOAD_EVENT),
            0x0d => Ok(Self::RAND_EVENT),
            0x0e => Ok(Self::USER_VAR_EVENT),
            0x0f => Ok(Self::FORMAT_DESCRIPTION_EVENT),
            0x10 => Ok(Self::XID_EVENT),
            0x11 => Ok(Self::BEGIN_LOAD_QUERY_EVENT),
            0x12 => Ok(Self::EXECUTE_LOAD_QUERY_EVENT),
            0x13 => Ok(Self::TABLE_MAP_EVENT),
            0x14 => Ok(Self::PRE_GA_WRITE_ROWS_EVENT),
            0x15 => Ok(Self::PRE_GA_UPDATE_ROWS_EVENT),
            0x16 => Ok(Self::PRE_GA_DELETE_ROWS_EVENT),
            0x17 => Ok(Self::WRITE_ROWS_EVENT_V1),
            0x18 => Ok(Self::UPDATE_ROWS_EVENT_V1),
            0x19 => Ok(Self::DELETE_ROWS_EVENT_V1),
            0x1a => Ok(Self::INCIDENT_EVENT),
            0x1b => Ok(Self::HEARTBEAT_EVENT),
            0x1c => Ok(Self::IGNORABLE_EVENT),
            0x1d => Ok(Self::ROWS_QUERY_EVENT),
            0x1e => Ok(Self::WRITE_ROWS_EVENT),
            0x1f => Ok(Self::UPDATE_ROWS_EVENT),
            0x20 => Ok(Self::DELETE_ROWS_EVENT),
            0x21 => Ok(Self::GTID_EVENT),
            0x22 => Ok(Self::ANONYMOUS_GTID_EVENT),
            0x23 => Ok(Self::PREVIOUS_GTIDS_EVENT),
            x => Err(UnknownEventType(x)),
        }
    }
}

my_bitflags! {
    EventFlags, u16,

    /// Binlog Event Flags
    pub struct EventFlags: u16 {
        /// Gets unset in the `FORMAT_DESCRIPTION_EVENT`
        /// when the file gets closed to detect broken binlogs.
        const LOG_EVENT_BINLOG_IN_USE_F = 0x0001;

        /// Unused.
        const LOG_EVENT_FORCED_ROTATE_F = 0x0002;

        /// event is thread specific (`CREATE TEMPORARY TABLE` ...).
        const LOG_EVENT_THREAD_SPECIFIC_F = 0x0004;

        /// Event doesn't need default database to be updated (`CREATE DATABASE`, ...).
        const LOG_EVENT_SUPPRESS_USE_F = 0x0008;

        /// Unused.
        const LOG_EVENT_UPDATE_TABLE_MAP_VERSION_F = 0x0010;

        /// Event is created by the slaves SQL-thread and shouldn't update the master-log pos.
        const LOG_EVENT_ARTIFICIAL_F = 0x0020;

        /// Event is created by the slaves IO-thread when written to the relay log.
        const LOG_EVENT_RELAY_LOG_F = 0x0040;

        /// Setting this flag will mark an event as Ignorable.
        const LOG_EVENT_IGNORABLE_F = 0x0080;

        /// Events with this flag are not filtered (e.g. on the current
        /// database) and are always written to the binary log regardless of
        /// filters.
        const LOG_EVENT_NO_FILTER_F = 0x0100;

        /// MTS: group of events can be marked to force its execution in isolation from
        /// any other Workers.
        const LOG_EVENT_MTS_ISOLATE_F = 0x0200;
    }
}

/// Binlog event.
///
/// For structs that aren't binlog events `event_size` and `fde` parameters are ignored
/// (one can use `FormatDescriptionEvent::new` constructor).
pub trait BinlogStruct {
    /// An event type, associated with this struct (if any).
    const EVENT_TYPE: Option<EventType>;

    /// Will read this struct from the given stream.
    ///
    /// *   implementation must error with `UnexpectedEof` if `event_size` is less than minimum
    ///     event size for this struct,
    /// *   implementation must error with (`Other`, `"bytes remaining on stream"`) if `event_size`
    ///     is greater than the event.
    ///
    /// Requires that if `Self::EVENT_TYPE` isn't `None`, then `event_size` and `data`
    /// are both without checksum-related suffix of length:
    ///
    /// *   `BINLOG_CHECKSUM_ALG_DESC_LEN + BINLOG_CHECKSUM_LEN` for `FormatDescriptionEvent`;
    /// *   `BINLOG_CHECKSUM_LEN` for other events.
    fn read<T: Read>(event_size: usize, fde: &FormatDescriptionEvent, input: T) -> io::Result<Self>
    where
        Self: Sized;

    /// Will write this struct to the given stream.
    ///
    /// # Notes
    ///
    /// *   implementation must error with `WriteZero` if field exceeds its maximum length.
    fn write<T: Write>(&self, version: BinlogVersion, output: T) -> io::Result<()>;

    /// Returns serialized length of this struct in bytes.
    ///
    /// *   implementation must truncate each field to its maximum length.
    fn len(&self, version: BinlogVersion) -> usize;
}

/// A binlog file starts with a Binlog File Header `[ fe 'bin' ]`.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub struct BinlogFileHeader;

impl BinlogFileHeader {
    /// Length of a binlog file header.
    pub const LEN: usize = 4;
    /// Value of a binlog file header.
    pub const VALUE: [u8; Self::LEN] = [0xfe, b'b', b'i', b'n'];
}

impl BinlogStruct for BinlogFileHeader {
    const EVENT_TYPE: Option<EventType> = None;

    /// Event size and post-header length will be ignored for this struct.
    ///
    /// # Note
    ///
    /// It'll return `InvalidData` if header != `Self::Value`.
    fn read<T: Read>(
        _event_size: usize,
        _fde: &FormatDescriptionEvent,
        mut input: T,
    ) -> io::Result<Self> {
        let mut buf = [0_u8; Self::LEN];
        input.read_exact(&mut buf)?;

        if buf != Self::VALUE {
            return Err(Error::new(InvalidData, "invalid binlog file header"));
        }

        Ok(Self)
    }

    fn write<T: Write>(&self, _version: BinlogVersion, mut output: T) -> io::Result<()> {
        output.write_all(&Self::VALUE)
    }

    fn len(&self, _version: BinlogVersion) -> usize {
        Self::LEN
    }
}

/// Reader for binlog events.
///
/// It'll maintain actual fde and must be used
/// to read binlog files and binlog event streams from server.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct EventStreamReader {
    fde: FormatDescriptionEvent,
}

impl EventStreamReader {
    /// Creates new instance.
    pub fn new(version: BinlogVersion) -> Self {
        Self {
            fde: FormatDescriptionEvent::new(version),
        }
    }

    /// Will read next event from the given stream using actual fde.
    pub fn read<T: Read>(&mut self, input: T) -> io::Result<Event> {
        let event = Event::read(0, &self.fde, input)?;

        // we'll redefine fde with an actual one
        if event.header.event_type.get() == Ok(EventType::FORMAT_DESCRIPTION_EVENT) {
            self.fde = match event.read_event::<FormatDescriptionEvent>() {
                Ok(mut fde) => {
                    fde.footer = event.footer;
                    fde
                }
                Err(err) => return Err(err),
            };
        }

        Ok(event)
    }
}

/// Binlog file.
///
/// It's an iterator over events in a binlog file.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct BinlogFile<T> {
    reader: EventStreamReader,
    read: T,
}

impl<T: Read> BinlogFile<T> {
    /// Creates new binlog file.
    ///
    /// It'll try to read binlog file header.
    pub fn new(version: BinlogVersion, mut read: T) -> io::Result<Self> {
        let reader = EventStreamReader::new(version);
        BinlogFileHeader::read(BinlogFileHeader::LEN, &reader.fde, &mut read)?;
        Ok(Self { reader, read })
    }
}

impl<T: Read> Iterator for BinlogFile<T> {
    type Item = io::Result<Event>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.reader.read(&mut self.read) {
            Ok(event) => Some(Ok(event)),
            Err(err) if err.kind() == UnexpectedEof => None,
            Err(err) => Some(Err(err)),
        }
    }
}

/// Parsed event data.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum EventData {
    UnknownEvent,
    /// Ignored by this implementation
    StartEventV3(Vec<u8>),
    QueryEvent(QueryEvent),
    StopEvent,
    RotateEvent(RotateEvent),
    IntvarEvent(IntvarEvent),
    /// Ignored by this implementation
    LoadEvent(Vec<u8>),
    SlaveEvent,
    CreateFileEvent(Vec<u8>),
    /// Ignored by this implementation
    AppendBlockEvent(Vec<u8>),
    /// Ignored by this implementation
    ExecLoadEvent(Vec<u8>),
    /// Ignored by this implementation
    DeleteFileEvent(Vec<u8>),
    /// Ignored by this implementation
    NewLoadEvent(Vec<u8>),
    RandEvent(RandEvent),
    UserVarEvent(UserVarEvent),
    FormatDescriptionEvent(FormatDescriptionEvent),
    XidEvent(XidEvent),
    BeginLoadQueryEvent(BeginLoadQueryEvent),
    ExecuteLoadQueryEvent(ExecuteLoadQueryEvent),
    TableMapEvent(TableMapEvent),
    /// Ignored by this implementation
    PreGaWriteRowsEvent(Vec<u8>),
    /// Ignored by this implementation
    PreGaUpdateRowsEvent(Vec<u8>),
    /// Ignored by this implementation
    PreGaDeleteRowsEvent(Vec<u8>),
    /// Ignored by this implementation
    WriteRowsEventV1(Vec<u8>),
    /// Ignored by this implementation
    UpdateRowsEventV1(Vec<u8>),
    /// Ignored by this implementation
    DeleteRowsEventV1(Vec<u8>),
    IncidentEvent(IncidentEvent),
    HeartbeatEvent,
    IgnorableEvent(Vec<u8>),
    RowsQueryEvent(RowsQueryEvent),
    WriteRowsEvent(WriteRowsEvent),
    UpdateRowsEvent(UpdateRowsEvent),
    DeleteRowsEvent(DeleteRowsEvent),
    /// Not yet implemented.
    GtidEvent(Vec<u8>),
    /// Not yet implemented.
    AnonymousGtidEvent(Vec<u8>),
    /// Not yet implemented.
    PreviousGtidsEvent(Vec<u8>),
    /// Not yet implemented.
    TransactionContextEvent(Vec<u8>),
    /// Not yet implemented.
    ViewChangeEvent(Vec<u8>),
    /// Not yet implemented.
    XaPrepareLogEvent(Vec<u8>),
    /// Not yet implemented.
    PartialUpdateRowsEvent(Vec<u8>),
}

impl EventData {
    /// Calls `BinlogStruct::write` for this variant.
    pub fn write<T: Write>(&self, version: BinlogVersion, mut output: T) -> io::Result<()> {
        match self {
            EventData::UnknownEvent => Ok(()),
            EventData::StartEventV3(ev) => output.write_all(&ev),
            EventData::QueryEvent(ev) => ev.write(version, output),
            EventData::StopEvent => Ok(()),
            EventData::RotateEvent(ev) => ev.write(version, output),
            EventData::IntvarEvent(ev) => ev.write(version, output),
            EventData::LoadEvent(ev) => output.write_all(&ev),
            EventData::SlaveEvent => Ok(()),
            EventData::CreateFileEvent(ev) => output.write_all(&ev),
            EventData::AppendBlockEvent(ev) => output.write_all(&ev),
            EventData::ExecLoadEvent(ev) => output.write_all(&ev),
            EventData::DeleteFileEvent(ev) => output.write_all(&ev),
            EventData::NewLoadEvent(ev) => output.write_all(&ev),
            EventData::RandEvent(ev) => ev.write(version, output),
            EventData::UserVarEvent(ev) => ev.write(version, output),
            EventData::FormatDescriptionEvent(ev) => ev.write(version, output),
            EventData::XidEvent(ev) => ev.write(version, output),
            EventData::BeginLoadQueryEvent(ev) => ev.write(version, output),
            EventData::ExecuteLoadQueryEvent(ev) => ev.write(version, output),
            EventData::TableMapEvent(ev) => ev.write(version, output),
            EventData::PreGaWriteRowsEvent(ev) => output.write_all(&ev),
            EventData::PreGaUpdateRowsEvent(ev) => output.write_all(&ev),
            EventData::PreGaDeleteRowsEvent(ev) => output.write_all(&ev),
            EventData::WriteRowsEventV1(ev) => output.write_all(&ev),
            EventData::UpdateRowsEventV1(ev) => output.write_all(&ev),
            EventData::DeleteRowsEventV1(ev) => output.write_all(&ev),
            EventData::IncidentEvent(ev) => ev.write(version, output),
            EventData::HeartbeatEvent => Ok(()),
            EventData::IgnorableEvent(ev) => output.write_all(&ev),
            EventData::RowsQueryEvent(ev) => ev.write(version, output),
            EventData::WriteRowsEvent(ev) => ev.write(version, output),
            EventData::UpdateRowsEvent(ev) => ev.write(version, output),
            EventData::DeleteRowsEvent(ev) => ev.write(version, output),
            EventData::GtidEvent(ev) => output.write_all(&ev),
            EventData::AnonymousGtidEvent(ev) => output.write_all(&ev),
            EventData::PreviousGtidsEvent(ev) => output.write_all(&ev),
            EventData::TransactionContextEvent(ev) => output.write_all(&ev),
            EventData::ViewChangeEvent(ev) => output.write_all(&ev),
            EventData::XaPrepareLogEvent(ev) => output.write_all(&ev),
            EventData::PartialUpdateRowsEvent(ev) => output.write_all(&ev),
        }
    }
}

/// Enumeration spcifying checksum algorithm used to encode a binary log event.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
#[allow(non_camel_case_types)]
#[repr(u8)]
pub enum BinlogChecksumAlg {
    /// Events are without checksum though its generator is checksum-capable New Master (NM).
    BINLOG_CHECKSUM_ALG_OFF = 0,
    /// CRC32 of zlib algorithm
    BINLOG_CHECKSUM_ALG_CRC32 = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
#[error("Unknown checksum algorithm {}", _0)]
#[repr(transparent)]
pub struct UnknownChecksumAlg(pub u8);

impl From<UnknownChecksumAlg> for u8 {
    fn from(x: UnknownChecksumAlg) -> Self {
        x.0
    }
}

impl TryFrom<u8> for BinlogChecksumAlg {
    type Error = UnknownChecksumAlg;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::BINLOG_CHECKSUM_ALG_OFF),
            1 => Ok(Self::BINLOG_CHECKSUM_ALG_CRC32),
            x => Err(UnknownChecksumAlg(x)),
        }
    }
}

/// Raw binlog event.
///
/// A binlog event starts with a Binlog Event header and is followed by a Binlog Event Type
/// specific data part.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct Event {
    /// Format description event.
    pub fde: FormatDescriptionEvent,
    /// Common header of an event.
    pub header: BinlogEventHeader,
    /// An event-type specific data.
    ///
    /// Checksum-related suffix is truncated:
    ///
    /// *   checksum algorithm description (for fde) will go to `footer`;
    /// *   checksum will go to `checksum`.
    pub data: Vec<u8>,
    /// Log event footer.
    pub footer: BinlogEventFooter,
    /// Event checksum.
    ///
    /// Makes sense only if checksum algorithm is defined in `footer`.
    pub checksum: [u8; BinlogEventFooter::BINLOG_CHECKSUM_LEN],
}

impl Event {
    /// Read event-type specific data as a binlog struct.
    pub fn read_event<T: BinlogStruct>(&self) -> io::Result<T> {
        BinlogStruct::read(
            // we'll use data.len() here because of truncated event footer
            BinlogEventHeader::LEN + self.data.len(),
            &self.fde,
            &*self.data,
        )
    }

    /// Reads event data. Returns `None` if event type is unknown.
    pub fn read_data(&self) -> io::Result<Option<EventData>> {
        use EventType::*;

        let event_type = match self.header.event_type.get() {
            Ok(event_type) => event_type,
            _ => return Ok(None),
        };

        let event_data = match event_type {
            ENUM_END_EVENT | UNKNOWN_EVENT => EventData::UnknownEvent,
            START_EVENT_V3 => EventData::StartEventV3(self.data.clone()),
            QUERY_EVENT => EventData::QueryEvent(self.read_event()?),
            STOP_EVENT => EventData::StopEvent,
            ROTATE_EVENT => EventData::RotateEvent(self.read_event()?),
            INTVAR_EVENT => EventData::IntvarEvent(self.read_event()?),
            LOAD_EVENT => EventData::LoadEvent(self.data.clone()),
            SLAVE_EVENT => EventData::SlaveEvent,
            CREATE_FILE_EVENT => EventData::CreateFileEvent(self.data.clone()),
            APPEND_BLOCK_EVENT => EventData::AppendBlockEvent(self.data.clone()),
            EXEC_LOAD_EVENT => EventData::ExecLoadEvent(self.data.clone()),
            DELETE_FILE_EVENT => EventData::DeleteFileEvent(self.data.clone()),
            NEW_LOAD_EVENT => EventData::NewLoadEvent(self.data.clone()),
            RAND_EVENT => EventData::RandEvent(self.read_event()?),
            USER_VAR_EVENT => EventData::UserVarEvent(self.read_event()?),
            FORMAT_DESCRIPTION_EVENT => {
                let mut fde: FormatDescriptionEvent = self.read_event()?;
                fde.footer = self.footer;
                EventData::FormatDescriptionEvent(fde)
            }
            XID_EVENT => EventData::XidEvent(self.read_event()?),
            BEGIN_LOAD_QUERY_EVENT => EventData::BeginLoadQueryEvent(self.read_event()?),
            EXECUTE_LOAD_QUERY_EVENT => EventData::ExecuteLoadQueryEvent(self.read_event()?),
            TABLE_MAP_EVENT => EventData::TableMapEvent(self.read_event()?),
            PRE_GA_WRITE_ROWS_EVENT => EventData::PreGaWriteRowsEvent(self.data.clone()),
            PRE_GA_UPDATE_ROWS_EVENT => EventData::PreGaUpdateRowsEvent(self.data.clone()),
            PRE_GA_DELETE_ROWS_EVENT => EventData::PreGaDeleteRowsEvent(self.data.clone()),
            WRITE_ROWS_EVENT_V1 => EventData::WriteRowsEventV1(self.data.clone()),
            UPDATE_ROWS_EVENT_V1 => EventData::UpdateRowsEventV1(self.data.clone()),
            DELETE_ROWS_EVENT_V1 => EventData::DeleteRowsEventV1(self.data.clone()),
            INCIDENT_EVENT => EventData::IncidentEvent(self.read_event()?),
            HEARTBEAT_EVENT => EventData::HeartbeatEvent,
            IGNORABLE_EVENT => EventData::IgnorableEvent(self.data.clone()),
            ROWS_QUERY_EVENT => EventData::RowsQueryEvent(self.read_event()?),
            WRITE_ROWS_EVENT => EventData::WriteRowsEvent(self.read_event()?),
            UPDATE_ROWS_EVENT => EventData::UpdateRowsEvent(self.read_event()?),
            DELETE_ROWS_EVENT => EventData::DeleteRowsEvent(self.read_event()?),
            GTID_EVENT => EventData::GtidEvent(self.data.clone()),
            ANONYMOUS_GTID_EVENT => EventData::AnonymousGtidEvent(self.data.clone()),
            PREVIOUS_GTIDS_EVENT => EventData::PreviousGtidsEvent(self.data.clone()),
            TRANSACTION_CONTEXT_EVENT => EventData::TransactionContextEvent(self.data.clone()),
            VIEW_CHANGE_EVENT => EventData::ViewChangeEvent(self.data.clone()),
            XA_PREPARE_LOG_EVENT => EventData::XaPrepareLogEvent(self.data.clone()),
            PARTIAL_UPDATE_ROWS_EVENT => EventData::PartialUpdateRowsEvent(self.data.clone()),
        };

        Ok(Some(event_data))
    }

    /// Calculates checksum for this event.
    pub fn calc_checksum(&self, alg: BinlogChecksumAlg) -> u32 {
        let is_fde = self.header.event_type.0 == EventType::FORMAT_DESCRIPTION_EVENT as u8;

        let mut hasher = crc32fast::Hasher::new();
        let mut header = [0_u8; BinlogEventHeader::LEN];
        self.header
            .write(
                self.fde
                    .binlog_version
                    .get()
                    .unwrap_or(BinlogVersion::Version4),
                &mut header[..],
            )
            .expect("should not fail");
        hasher.update(&header);
        hasher.update(&self.data);
        if is_fde {
            hasher.update(&[alg as u8][..]);
        }
        hasher.finalize()
    }
}

impl BinlogStruct for Event {
    const EVENT_TYPE: Option<EventType> = None;

    /// `event_size` will be ignored.
    fn read<T: Read>(
        _event_size: usize,
        fde: &FormatDescriptionEvent,
        mut input: T,
    ) -> io::Result<Self> {
        let version = fde.binlog_version.get().unwrap_or(BinlogVersion::Version4);
        let binlog_header_len = BinlogEventHeader::len(version);
        let mut fde = fde.clone();

        let header = BinlogEventHeader::read(BinlogEventHeader::len(version), &fde, &mut input)?;

        let mut data = vec![0_u8; (S(header.event_size as usize) - S(binlog_header_len)).0];
        input.read_exact(&mut data).unwrap();

        let is_fde = header.event_type.0 == EventType::FORMAT_DESCRIPTION_EVENT as u8;
        let mut bytes_to_truncate = 0;
        let mut checksum = [0_u8; BinlogEventFooter::BINLOG_CHECKSUM_LEN];

        let footer = if is_fde {
            let footer = BinlogEventFooter::read(&data)?;
            if !footer.checksum_alg.is_none() {
                // truncate checksum algorithm description
                bytes_to_truncate += BinlogEventFooter::BINLOG_CHECKSUM_ALG_DESC_LEN;
            }
            // We'll update dummy fde footer
            fde.footer = footer;
            footer
        } else {
            fde.footer
        };

        // fde will always contain checksum (see WL#2540)
        let contains_checksum = !footer.checksum_alg.is_none()
            && (is_fde || footer.checksum_alg != Some(RawField::new(0)));

        if contains_checksum {
            // truncate checksum
            bytes_to_truncate += BinlogEventFooter::BINLOG_CHECKSUM_LEN;
            checksum.copy_from_slice(&data[data.len() - BinlogEventFooter::BINLOG_CHECKSUM_LEN..]);
        }

        data.truncate(data.len() - bytes_to_truncate);

        Ok(Self {
            header,
            fde,
            data,
            footer,
            checksum,
        })
    }

    fn write<T: Write>(&self, version: BinlogVersion, mut output: T) -> io::Result<()> {
        let is_fde = self.header.event_type.0 == EventType::FORMAT_DESCRIPTION_EVENT as u8;
        let mut output = output.limit(S(self.len(version)));

        self.header.write(version, &mut output)?;
        output.write_all(&self.data)?;

        match self.footer.get_checksum_alg() {
            Ok(Some(alg)) => {
                if is_fde {
                    output.write_u8(alg as u8)?;
                }
                if alg != BinlogChecksumAlg::BINLOG_CHECKSUM_ALG_OFF || is_fde {
                    output.write_u32::<LittleEndian>(self.calc_checksum(alg))?;
                }
            }
            _ => (),
        }

        Ok(())
    }

    fn len(&self, version: BinlogVersion) -> usize {
        let is_fde = self.header.event_type.0 == EventType::FORMAT_DESCRIPTION_EVENT as u8;
        let mut len = S(0);

        len += S(BinlogEventHeader::len(version));
        len += S(self.data.len());
        match self.footer.get_checksum_alg() {
            Ok(Some(alg)) => {
                if is_fde {
                    len += S(BinlogEventFooter::BINLOG_CHECKSUM_ALG_DESC_LEN);
                }
                if is_fde || alg != BinlogChecksumAlg::BINLOG_CHECKSUM_ALG_OFF {
                    len += S(BinlogEventFooter::BINLOG_CHECKSUM_LEN);
                }
            }
            _ => (),
        }

        min(len.0, u32::MAX as usize - BinlogEventHeader::len(version))
    }
}

/// The binlog event header starts each event and is 19 bytes long assuming binlog version >= 4.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct BinlogEventHeader {
    /// Seconds since unix epoch.
    pub timestamp: u32,
    /// Binlog Event Type.
    ///
    /// This field contains raw value. Use [`Self::get_event_type()`] to get the actual event type.
    pub event_type: RawField<u8, UnknownEventType, EventType>,
    /// Server-id of the originating mysql-server.
    ///
    /// Used to filter out events in circular replication.
    pub server_id: u32,
    /// Size of the event (header, post-header, body).
    pub event_size: u32,
    /// Position of the next event.
    pub log_pos: u32,
    /// Binlog Event Flag.
    ///
    /// This field contains raw value. Use [`Self::get_flags()`] to get the actual flags.
    pub flags: RawFlags<EventFlags>,
}

impl BinlogEventHeader {
    /// Binlog event header length for version >= 4.
    pub const LEN: usize = 19;

    /// Returns binlog event header length.
    pub fn len(_version: BinlogVersion) -> usize {
        Self::LEN
    }
}

impl BinlogStruct for BinlogEventHeader {
    const EVENT_TYPE: Option<EventType> = None;

    /// Event size will be ignored for this struct.
    fn read<T: Read>(
        _event_size: usize,
        _fde: &FormatDescriptionEvent,
        mut input: T,
    ) -> io::Result<Self> {
        let timestamp = input.read_u32::<LittleEndian>()?;
        let event_type = input.read_u8()?;
        let server_id = input.read_u32::<LittleEndian>()?;
        let event_size = input.read_u32::<LittleEndian>()?;
        let log_pos = input.read_u32::<LittleEndian>()?;
        let flags = input.read_u16::<LittleEndian>()?;

        Ok(Self {
            timestamp,
            event_type: RawField::new(event_type),
            server_id,
            event_size,
            log_pos,
            flags: RawFlags(flags),
        })
    }

    fn write<T: Write>(&self, _version: BinlogVersion, mut output: T) -> io::Result<()> {
        output.write_u32::<LittleEndian>(self.timestamp)?;
        output.write_u8(self.event_type.0)?;
        output.write_u32::<LittleEndian>(self.server_id)?;
        output.write_u32::<LittleEndian>(self.event_size)?;
        output.write_u32::<LittleEndian>(self.log_pos)?;
        output.write_u16::<LittleEndian>(self.flags.0)?;
        Ok(())
    }

    fn len(&self, version: BinlogVersion) -> usize {
        Self::len(version)
    }
}

/// Binlog event footer.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub struct BinlogEventFooter {
    /// Raw checksum algorithm description.
    pub checksum_alg: Option<RawField<u8, UnknownChecksumAlg, BinlogChecksumAlg>>,
}

impl BinlogEventFooter {
    /// Length of the checksum algorithm description.
    pub const BINLOG_CHECKSUM_ALG_DESC_LEN: usize = 1;
    /// Length of the checksum.
    pub const BINLOG_CHECKSUM_LEN: usize = 4;
    /// Minimum MySql version that supports checksums.
    pub const CHECKSUM_VERSION_PRODUCT: (u8, u8, u8) = (5, 6, 1);

    /// Returns parsed checksum algorithm, or raw value if algorithm is unknown.
    pub fn get_checksum_alg(&self) -> Result<Option<BinlogChecksumAlg>, UnknownChecksumAlg> {
        self.checksum_alg.as_ref().map(RawField::get).transpose()
    }

    /// Reads binlog event footer from the given buffer.
    ///
    /// Requires that buf contains `FormatDescriptionEvent` data.
    pub fn read(buf: &[u8]) -> io::Result<Self> {
        let checksum_alg = if buf.len()
            >= FormatDescriptionEvent::SERVER_VER_OFFSET + FormatDescriptionEvent::SERVER_VER_LEN
        {
            let mut server_version = vec![0_u8; FormatDescriptionEvent::SERVER_VER_LEN];
            (&buf[FormatDescriptionEvent::SERVER_VER_OFFSET..]).read_exact(&mut server_version)?;
            server_version[FormatDescriptionEvent::SERVER_VER_LEN - 1] = 0;
            let version = crate::misc::split_version(&server_version);
            if version < Self::CHECKSUM_VERSION_PRODUCT {
                None
            } else {
                let offset = buf.len()
                    - (BinlogEventFooter::BINLOG_CHECKSUM_ALG_DESC_LEN
                        + BinlogEventFooter::BINLOG_CHECKSUM_LEN);
                Some(buf[offset])
            }
        } else {
            None
        };

        Ok(Self {
            checksum_alg: checksum_alg.map(RawField::new),
        })
    }
}

impl Default for BinlogEventFooter {
    fn default() -> Self {
        BinlogEventFooter {
            checksum_alg: Some(RawField::new(
                BinlogChecksumAlg::BINLOG_CHECKSUM_ALG_OFF as u8,
            )),
        }
    }
}

/// A wrapper for 50-bytes array.
#[derive(Clone)]
pub struct RawServerVersion(pub [u8; FormatDescriptionEvent::SERVER_VER_LEN]);

impl fmt::Debug for RawServerVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (&self.0[..]).fmt(f)
    }
}

impl AsRef<[u8]> for RawServerVersion {
    fn as_ref(&self) -> &[u8] {
        &self.0[..]
    }
}

impl PartialEq for RawServerVersion {
    fn eq(&self, other: &Self) -> bool {
        &self.0[..] == &other.0[..]
    }
}

impl Eq for RawServerVersion {}

impl Hash for RawServerVersion {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (&self.0[..]).hash(state);
    }
}

/// A format description event is the first event of a binlog for binlog-version 4.
///
/// It describes how the other events are layed out.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct FormatDescriptionEvent {
    /// Version of this binlog format.
    pub binlog_version: RawField<u16, UnknownBinlogVersion, BinlogVersion>,

    /// Version of the MySQL Server that created the binlog (len=50).
    ///
    /// The string is evaluted to apply work-arounds in the slave.
    pub server_version: RawText<RawServerVersion>,

    /// Seconds since Unix epoch when the binlog was created.
    pub create_timestamp: u32,

    // pub event_header_length: u8, // It's always 19, so ignored.
    /// An array indexed by Binlog Event Type - 1 to extract the length of the event specific
    /// header.
    ///
    /// Use [`Self::get_event_type_header_length`] to get header length for particular event type.
    pub event_type_header_lengths: Vec<u8>,

    /// This event structure also stores a footer containig checksum algorithm description.
    ///
    /// # Note
    ///
    /// Footer must be assigned manualy after `Self::read`
    pub footer: BinlogEventFooter,
}

impl FormatDescriptionEvent {
    /// Length of a server version string.
    pub const SERVER_VER_LEN: usize = 50;
    /// Offset of a server version string.
    pub const SERVER_VER_OFFSET: usize = 2;

    // Other format-related constants
    /// Length of a query event post-header, where 3.23, 4.x and 5.0 agree.
    pub const QUERY_HEADER_MINIMAL_LEN: usize = (4 + 4 + 1 + 2);
    /// Length of a query event post-header, where 5.0 differs: 2 for length of N-bytes vars.
    pub const QUERY_HEADER_LEN: usize = Self::QUERY_HEADER_MINIMAL_LEN + 2;
    /// Length of a stop event post-header.
    pub const STOP_HEADER_LEN: usize = 0;
    /// Length of a start event post-header.
    pub const START_V3_HEADER_LEN: usize = 2 + Self::SERVER_VER_LEN + 4;
    /// Length of a rotate event post-header.
    pub const ROTATE_HEADER_LEN: usize = 8;
    /// Length of an intvar event post-header.
    pub const INTVAR_HEADER_LEN: usize = 0;
    /// Length of an append block event post-header.
    pub const APPEND_BLOCK_HEADER_LEN: usize = 4;
    /// Length of a delete file event post-header.
    pub const DELETE_FILE_HEADER_LEN: usize = 4;
    /// Length of a rand event post-header.
    pub const RAND_HEADER_LEN: usize = 0;
    /// Length of a user var event post-header.
    pub const USER_VAR_HEADER_LEN: usize = 0;
    /// Length of a fde event post-header.
    pub const FORMAT_DESCRIPTION_HEADER_LEN: usize =
        (Self::START_V3_HEADER_LEN + EventType::ENUM_END_EVENT as usize);
    /// Length of a xid event post-header.
    pub const XID_HEADER_LEN: usize = 0;
    /// Length of a begin load query event post-header.
    pub const BEGIN_LOAD_QUERY_HEADER_LEN: usize = Self::APPEND_BLOCK_HEADER_LEN;
    /// Length of a v1 rows query event post-header.
    pub const ROWS_HEADER_LEN_V1: usize = 8;
    /// Length of a table map event post-header.
    pub const TABLE_MAP_HEADER_LEN: usize = 8;
    /// Length of an execute load query event extra header.
    pub const EXECUTE_LOAD_QUERY_EXTRA_HEADER_LEN: usize = (4 + 4 + 4 + 1);
    /// Length of an execute load query event post-header.
    pub const EXECUTE_LOAD_QUERY_HEADER_LEN: usize =
        (Self::QUERY_HEADER_LEN + Self::EXECUTE_LOAD_QUERY_EXTRA_HEADER_LEN);
    /// Length of an incident event post-header.
    pub const INCIDENT_HEADER_LEN: usize = 2;
    /// Length of a heartbeat event post-header.
    pub const HEARTBEAT_HEADER_LEN: usize = 0;
    /// Length of an ignorable event post-header.
    pub const IGNORABLE_HEADER_LEN: usize = 0;
    /// Length of a rows events post-header.
    pub const ROWS_HEADER_LEN_V2: usize = 10;
    /// Length of a gtid events post-header.
    pub const GTID_HEADER_LEN: usize = 42;
    /// Length of an incident event post-header.
    pub const TRANSACTION_CONTEXT_HEADER_LEN: usize = 18;
    /// Length of a view change event post-header.
    pub const VIEW_CHANGE_HEADER_LEN: usize = 52;
    /// Length of a xa prepare event post-header.
    pub const XA_PREPARE_HEADER_LEN: usize = 0;

    /// Creates format description event suitable for `FormatDescriptionEvent::read`.
    pub fn new(binlog_version: BinlogVersion) -> Self {
        Self {
            binlog_version: RawField::new(binlog_version as u16),
            server_version: RawText(RawServerVersion([0_u8; Self::SERVER_VER_LEN])),
            create_timestamp: 0,
            event_type_header_lengths: Vec::new(),
            footer: Default::default(),
        }
    }

    /// Returns header length for the given event type, if defined.
    pub fn get_event_type_header_length(&self, event_type: EventType) -> u8 {
        if event_type == EventType::UNKNOWN_EVENT {
            return 0;
        }

        self.event_type_header_lengths
            .get(usize::from(event_type as u8).saturating_sub(1))
            .copied()
            .unwrap_or_else(|| match event_type {
                EventType::UNKNOWN_EVENT => 0,
                EventType::START_EVENT_V3 => Self::START_V3_HEADER_LEN,
                EventType::QUERY_EVENT => Self::QUERY_HEADER_LEN,
                EventType::STOP_EVENT => Self::STOP_HEADER_LEN,
                EventType::ROTATE_EVENT => Self::ROTATE_HEADER_LEN,
                EventType::INTVAR_EVENT => Self::INTVAR_HEADER_LEN,
                EventType::LOAD_EVENT => 0,
                EventType::SLAVE_EVENT => 0,
                EventType::CREATE_FILE_EVENT => 0,
                EventType::APPEND_BLOCK_EVENT => Self::APPEND_BLOCK_HEADER_LEN,
                EventType::EXEC_LOAD_EVENT => 0,
                EventType::DELETE_FILE_EVENT => Self::DELETE_FILE_HEADER_LEN,
                EventType::NEW_LOAD_EVENT => 0,
                EventType::RAND_EVENT => Self::RAND_HEADER_LEN,
                EventType::USER_VAR_EVENT => Self::USER_VAR_HEADER_LEN,
                EventType::FORMAT_DESCRIPTION_EVENT => Self::FORMAT_DESCRIPTION_HEADER_LEN,
                EventType::XID_EVENT => Self::XID_HEADER_LEN,
                EventType::BEGIN_LOAD_QUERY_EVENT => Self::BEGIN_LOAD_QUERY_HEADER_LEN,
                EventType::EXECUTE_LOAD_QUERY_EVENT => Self::EXECUTE_LOAD_QUERY_HEADER_LEN,
                EventType::TABLE_MAP_EVENT => Self::TABLE_MAP_HEADER_LEN,
                EventType::PRE_GA_WRITE_ROWS_EVENT => 0,
                EventType::PRE_GA_UPDATE_ROWS_EVENT => 0,
                EventType::PRE_GA_DELETE_ROWS_EVENT => 0,
                EventType::WRITE_ROWS_EVENT_V1 => Self::ROWS_HEADER_LEN_V1,
                EventType::UPDATE_ROWS_EVENT_V1 => Self::ROWS_HEADER_LEN_V1,
                EventType::DELETE_ROWS_EVENT_V1 => Self::ROWS_HEADER_LEN_V1,
                EventType::INCIDENT_EVENT => Self::INCIDENT_HEADER_LEN,
                EventType::HEARTBEAT_EVENT => 0,
                EventType::IGNORABLE_EVENT => Self::IGNORABLE_HEADER_LEN,
                EventType::ROWS_QUERY_EVENT => Self::IGNORABLE_HEADER_LEN,
                EventType::WRITE_ROWS_EVENT => Self::ROWS_HEADER_LEN_V2,
                EventType::UPDATE_ROWS_EVENT => Self::ROWS_HEADER_LEN_V2,
                EventType::DELETE_ROWS_EVENT => Self::ROWS_HEADER_LEN_V2,
                EventType::GTID_EVENT => Self::GTID_HEADER_LEN,
                EventType::ANONYMOUS_GTID_EVENT => Self::GTID_HEADER_LEN,
                EventType::PREVIOUS_GTIDS_EVENT => Self::IGNORABLE_HEADER_LEN,
                EventType::TRANSACTION_CONTEXT_EVENT => Self::TRANSACTION_CONTEXT_HEADER_LEN,
                EventType::VIEW_CHANGE_EVENT => Self::VIEW_CHANGE_HEADER_LEN,
                EventType::XA_PREPARE_LOG_EVENT => Self::XA_PREPARE_HEADER_LEN,
                EventType::PARTIAL_UPDATE_ROWS_EVENT => Self::ROWS_HEADER_LEN_V2,
                EventType::ENUM_END_EVENT => 0,
            } as u8)
    }
}

impl BinlogStruct for FormatDescriptionEvent {
    const EVENT_TYPE: Option<EventType> = Some(EventType::FORMAT_DESCRIPTION_EVENT);

    fn read<T: Read>(
        event_size: usize,
        _fde: &FormatDescriptionEvent,
        mut input: T,
    ) -> io::Result<Self> {
        let mut input = input.limit(S(event_size) - S(BinlogEventHeader::LEN));

        let binlog_version = input.read_u16::<LittleEndian>()?;

        let mut server_version = [0_u8; Self::SERVER_VER_LEN];
        input.read_exact(&mut server_version[..])?;

        let create_timestamp = input.read_u32::<LittleEndian>()?;

        input.read_u8()?; // skip event_header_length

        let mut event_type_header_lengths = vec![0_u8; input.get_limit()];
        input.read_exact(&mut event_type_header_lengths)?;

        Ok(Self {
            binlog_version: RawField::new(binlog_version),
            server_version: RawText(RawServerVersion(server_version)),
            create_timestamp,
            event_type_header_lengths,
            footer: Default::default(),
        })
    }

    fn write<T: Write>(&self, version: BinlogVersion, mut output: T) -> io::Result<()> {
        let mut output = output.limit(S(self.len(version)));

        output.write_u16::<LittleEndian>(self.binlog_version.0)?;
        output.write_all(&(self.server_version.0).0)?;
        output.write_u32::<LittleEndian>(self.create_timestamp)?;
        output.write_u8(BinlogEventHeader::LEN as u8)?;
        output.write_all(&self.event_type_header_lengths)?;

        Ok(())
    }

    fn len(&self, version: BinlogVersion) -> usize {
        let mut len = S(0);

        len += S(2);
        len += S(Self::SERVER_VER_LEN);
        len += S(4);
        len += S(1);
        len += S(self.event_type_header_lengths.len());

        min(len.0, u32::MAX as usize - BinlogEventHeader::len(version))
    }
}

/// The rotate event is added to the binlog as last event
/// to tell the reader what binlog to request next.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct RotateEvent {
    // post-header
    /// Only available if binlog version > 1 (zero otherwise).
    pub position: u64,

    // payload
    /// Name of the next binlog.
    pub name: RawText,
}

impl BinlogStruct for RotateEvent {
    const EVENT_TYPE: Option<EventType> = Some(EventType::ROTATE_EVENT);

    fn read<T: Read>(
        event_size: usize,
        fde: &FormatDescriptionEvent,
        mut input: T,
    ) -> io::Result<Self> {
        let version = fde.binlog_version.get().unwrap_or(BinlogVersion::Version4);
        let mut input = input.limit(S(event_size) - S(BinlogEventHeader::len(version)));

        let position = input.read_u64::<LittleEndian>()?;

        let mut name = vec![0_u8; input.get_limit()];
        input.read_exact(&mut name)?;

        Ok(Self {
            position,
            name: RawText(name),
        })
    }

    fn write<T: Write>(&self, version: BinlogVersion, mut output: T) -> io::Result<()> {
        let mut output = output.limit(S(self.len(version)));

        output.write_u64::<LittleEndian>(self.position)?;
        output.write_all(&self.name.0)?;

        Ok(())
    }

    fn len(&self, version: BinlogVersion) -> usize {
        let mut len = S(0);

        len += S(8);
        len += S(self.name.0.len());

        min(len.0, u32::MAX as usize - BinlogEventHeader::len(version))
    }
}

/// A query event is created for each query that modifies the database, unless the query
/// is logged row-based.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct QueryEvent {
    // post-header fields
    /// The ID of the thread that issued this statement. It is needed for temporary tables.
    pub thread_id: u32,
    /// The time from when the query started to when it was logged in the binlog, in seconds.
    pub execution_time: u32,
    /// Error code generated by the master. If the master fails, the slave will fail with
    /// the same error code.
    pub error_code: u16,

    // payload
    /// Zero or more status variables (`status_vars_length` bytes).
    ///
    /// Each status variable consists of one byte identifying the variable stored, followed
    /// by the value of the variable. Please consult the MySql documentation.
    ///
    /// Only available if binlog version >= 4 (empty otherwise).
    pub status_vars: StatusVars,
    /// The currently selected database name (`schema-length` bytes).
    pub schema: RawText,
    /// The SQL query.
    pub query: RawText,
}

impl BinlogStruct for QueryEvent {
    const EVENT_TYPE: Option<EventType> = Some(EventType::QUERY_EVENT);

    fn read<T: Read>(
        event_size: usize,
        fde: &FormatDescriptionEvent,
        mut input: T,
    ) -> io::Result<Self> {
        let version = fde.binlog_version.get().unwrap_or(BinlogVersion::Version4);
        let mut input = input.limit(S(event_size) - S(BinlogEventHeader::len(version)));

        let post_header_len = fde.get_event_type_header_length(Self::EVENT_TYPE.unwrap());

        let thread_id = input.read_u32::<LittleEndian>()?;
        let execution_time = input.read_u32::<LittleEndian>()?;
        let schema_len = input.read_u8()? as usize;
        let error_code = input.read_u16::<LittleEndian>()?;

        let status_vars_len = input.read_u16::<LittleEndian>()? as usize;

        for _ in 0..(post_header_len.saturating_sub(4 + 4 + 1 + 2 + 2)) {
            input.read_u8()?;
        }

        let mut status_vars = vec![0_u8; status_vars_len];
        input.read_exact(&mut status_vars)?;

        let mut schema = vec![0_u8; schema_len];
        input.read_exact(&mut schema)?;

        input.read_u8()?;

        let mut query = vec![0_u8; input.get_limit()];
        input.read_exact(&mut query)?;

        if input.get_limit() > 0 {
            return Err(Error::new(Other, "bytes remaining on stream"));
        }

        Ok(Self {
            thread_id,
            execution_time,
            error_code,
            status_vars: StatusVars(status_vars),
            schema: RawText(schema),
            query: RawText(query),
        })
    }

    fn write<T: Write>(&self, version: BinlogVersion, mut output: T) -> io::Result<()> {
        let mut output = output.limit(S(self.len(version)));

        let schema_len = min(self.schema.0.len(), u8::MAX as usize);
        let status_vars_len = min(self.status_vars.0.len(), u16::MAX as usize);

        output.write_u32::<LittleEndian>(self.thread_id)?;
        output.write_u32::<LittleEndian>(self.execution_time)?;
        output.write_u8(schema_len as u8)?;
        output.write_u16::<LittleEndian>(self.error_code)?;
        output.write_u16::<LittleEndian>(status_vars_len as u16)?;
        output
            .limit(S(status_vars_len))
            .write_all(&self.status_vars.0)?;
        output.limit(S(schema_len)).write_all(&self.schema.0)?;
        output.write_u8(0)?;
        output.write_all(&self.query.0)?;

        Ok(())
    }

    fn len(&self, version: BinlogVersion) -> usize {
        let mut len = S(0);

        len += S(4);
        len += S(4);
        len += S(1);
        len += S(2);
        len += S(2);
        len += S(min(self.status_vars.0.len(), u16::MAX as usize));
        len += S(min(self.schema.0.len(), u8::MAX as usize));
        len += S(1);
        len += S(self.query.0.len());

        min(len.0, u32::MAX as usize - BinlogEventHeader::len(version))
    }
}

/// Binlog query event status vars keys.
#[repr(u8)]
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum StatusVarKey {
    /// Contains `Flags2` flags.
    Flags2 = 0,
    /// Contains `SqlMode` flags.
    SqlMode,
    /// Contains values in the following order:
    ///
    /// *   1 byte `length`,
    /// *   `length` bytes catalog,
    /// *   NULL byte.
    ///
    /// `length + 2` bytes in total.
    Catalog,
    /// Contains values in the following order:
    ///
    /// *   2 bytes unsigned little-endian auto_increment_increment,
    /// *   2 bytes unsigned little-endian auto_increment_offset.
    ///
    /// Four bytes in total.
    AutoIncrement,
    /// Contains values in the following order:
    ///
    /// *   2 bytes unsigned little-endian character_set_client,
    /// *   2 bytes unsigned little-endian collation_connection,
    /// *   2 bytes unsigned little-endian collation_server.
    ///
    /// Six bytes in total.
    Charset,
    /// Contains values in the following order:
    ///
    /// *   1 byte `length`,
    /// *   `length` bytes timezone.
    ///
    /// `length + 1` bytes in total.
    TimeZone,
    /// Contains values in the following order:
    ///
    /// *   1 byte `length`,
    /// *   `length` bytes catalog.
    ///
    /// `length + 1` bytes in total.
    CatalogNz,
    /// Contains 2 bytes code identifying a table of month and day names.
    ///
    /// The mapping from codes to languages is defined in sql_locale.cc.
    LcTimeNames,
    /// Contains 2 bytes value of the collation_database system variable.
    CharsetDatabase,
    /// Contains 8 bytes value of the table map that is to be updated
    /// by the multi-table update query statement.
    TableMapForUpdate,
    /// Contains 4 bytes bitfield.
    MasterDataWritten,
    /// Contains values in the following order:
    ///
    /// *   1 byte `user_length`,
    /// *   `user_length` bytes user,
    /// *   1 byte `host_length`,
    /// *   `host_length` bytes host.
    ///
    /// `user_length + host_length + 2` bytes in total.
    Invoker,
    /// Contains values in the following order:
    ///
    /// *   1 byte `count`,
    /// *   `count` times:
    ///     *   null-terminated db_name.
    ///
    /// `1 + db_names_lens.sum()` bytes in total.
    UpdatedDbNames,
    /// Contains 3 bytes unsigned little-endian integer.
    Microseconds,
    CommitTs,
    CommitTs2,
    /// Contains 1 byte boolean.
    ExplicitDefaultsForTimestamp,
    /// Contains 8 bytes unsigned little-endian integer carrying xid info of 2pc-aware
    /// (recoverable) DDL queries.
    DdlLoggedWithXid,
    /// Contains 2 bytes unsigned little-endian integer carrying
    /// the default collation for the utf8mb4 character set.
    DefaultCollationForUtf8mb4,
    /// Contains 1 byte value.
    SqlRequirePrimaryKey,
    /// Contains 1 byte value.
    DefaultTableEncryption,
}

impl TryFrom<u8> for StatusVarKey {
    type Error = u8;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(StatusVarKey::Flags2),
            1 => Ok(StatusVarKey::SqlMode),
            2 => Ok(StatusVarKey::Catalog),
            3 => Ok(StatusVarKey::AutoIncrement),
            4 => Ok(StatusVarKey::Charset),
            5 => Ok(StatusVarKey::TimeZone),
            6 => Ok(StatusVarKey::CatalogNz),
            7 => Ok(StatusVarKey::LcTimeNames),
            8 => Ok(StatusVarKey::CharsetDatabase),
            9 => Ok(StatusVarKey::TableMapForUpdate),
            10 => Ok(StatusVarKey::MasterDataWritten),
            11 => Ok(StatusVarKey::Invoker),
            12 => Ok(StatusVarKey::UpdatedDbNames),
            13 => Ok(StatusVarKey::Microseconds),
            14 => Ok(StatusVarKey::CommitTs),
            15 => Ok(StatusVarKey::CommitTs2),
            16 => Ok(StatusVarKey::ExplicitDefaultsForTimestamp),
            17 => Ok(StatusVarKey::DdlLoggedWithXid),
            18 => Ok(StatusVarKey::DefaultCollationForUtf8mb4),
            19 => Ok(StatusVarKey::SqlRequirePrimaryKey),
            20 => Ok(StatusVarKey::DefaultTableEncryption),
            x => Err(x),
        }
    }
}

/// Status variable value.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum StatusVarVal<'a> {
    Flags2(RawFlags<crate::constants::Flags2>),
    SqlMode(RawFlags<crate::constants::SqlMode>),
    /// Ignored by this implementation.
    Catalog(&'a [u8]),
    AutoIncrement {
        increment: u16,
        offset: u16,
    },
    Charset {
        charset_client: u16,
        collation_connection: u16,
        collation_server: u16,
    },
    /// Will be empty if timezone length is `0`.
    TimeZone(RawText<&'a [u8]>),
    /// Will be empty if timezone length is `0`.
    CatalogNz(RawText<&'a [u8]>),
    LcTimeNames(u16),
    CharsetDatabase(u16),
    TableMapForUpdate(u64),
    MasterDataWritten([u8; 4]),
    Invoker {
        username: RawText<&'a [u8]>,
        hostname: RawText<&'a [u8]>,
    },
    UpdatedDbNames(Vec<RawText<&'a [u8]>>),
    Microseconds(u32),
    /// Ignored.
    CommitTs(&'a [u8]),
    /// Ignored.
    CommitTs2(&'a [u8]),
    /// `0` is interpreted as `false` and everything else as `true`.
    ExplicitDefaultsForTimestamp(bool),
    DdlLoggedWithXid(u64),
    DefaultCollationForUtf8mb4(u16),
    SqlRequirePrimaryKey(u8),
    DefaultTableEncryption(u8),
}

/// Raw status variable.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct StatusVar<'a> {
    /// Status variable key.
    key: StatusVarKey,
    /// Raw value of a status variable. Use `Self::get_value`.
    value: &'a [u8],
}

impl StatusVar<'_> {
    /// Returns parsed value of this status variable, or raw value in case of error.
    pub fn get_value(&self) -> Result<StatusVarVal, &[u8]> {
        match self.key {
            StatusVarKey::Flags2 => {
                let mut read = self.value;
                read.read_u32::<LittleEndian>()
                    .map(RawFlags)
                    .map(StatusVarVal::Flags2)
                    .map_err(|_| self.value)
            }
            StatusVarKey::SqlMode => {
                let mut read = self.value;
                read.read_u64::<LittleEndian>()
                    .map(RawFlags)
                    .map(StatusVarVal::SqlMode)
                    .map_err(|_| self.value)
            }
            StatusVarKey::Catalog => Ok(StatusVarVal::Catalog(self.value)),
            StatusVarKey::AutoIncrement => {
                let mut read = self.value;
                let increment = read.read_u16::<LittleEndian>().map_err(|_| self.value)?;
                let offset = read.read_u16::<LittleEndian>().map_err(|_| self.value)?;
                Ok(StatusVarVal::AutoIncrement { increment, offset })
            }
            StatusVarKey::Charset => {
                let mut read = self.value;
                let charset_client = read.read_u16::<LittleEndian>().map_err(|_| self.value)?;
                let collation_connection =
                    read.read_u16::<LittleEndian>().map_err(|_| self.value)?;
                let collation_server = read.read_u16::<LittleEndian>().map_err(|_| self.value)?;
                Ok(StatusVarVal::Charset {
                    charset_client,
                    collation_connection,
                    collation_server,
                })
            }
            StatusVarKey::TimeZone => {
                let mut read = self.value;
                let len = read.read_u8().map_err(|_| self.value)? as usize;
                let text = read.get(..len).ok_or(self.value)?;
                Ok(StatusVarVal::TimeZone(RawText(text)))
            }
            StatusVarKey::CatalogNz => {
                let mut read = self.value;
                let len = read.read_u8().map_err(|_| self.value)? as usize;
                let text = read.get(..len).ok_or(self.value)?;
                Ok(StatusVarVal::CatalogNz(RawText(text)))
            }
            StatusVarKey::LcTimeNames => {
                let mut read = self.value;
                let val = read.read_u16::<LittleEndian>().map_err(|_| self.value)?;
                Ok(StatusVarVal::LcTimeNames(val))
            }
            StatusVarKey::CharsetDatabase => {
                let mut read = self.value;
                let val = read.read_u16::<LittleEndian>().map_err(|_| self.value)?;
                Ok(StatusVarVal::CharsetDatabase(val))
            }
            StatusVarKey::TableMapForUpdate => {
                let mut read = self.value;
                let val = read.read_u64::<LittleEndian>().map_err(|_| self.value)?;
                Ok(StatusVarVal::TableMapForUpdate(val))
            }
            StatusVarKey::MasterDataWritten => {
                let mut read = self.value;
                let mut val = [0u8; 4];
                read.read_exact(&mut val).map_err(|_| self.value)?;
                Ok(StatusVarVal::MasterDataWritten(val))
            }
            StatusVarKey::Invoker => {
                let mut read = self.value;

                let len = read.read_u8().map_err(|_| self.value)? as usize;
                let username = read.get(..len).ok_or(self.value)?;
                read = &read[len..];

                let len = read.read_u8().map_err(|_| self.value)? as usize;
                let hostname = read.get(..len).ok_or(self.value)?;

                Ok(StatusVarVal::Invoker {
                    username: RawText(username),
                    hostname: RawText(hostname),
                })
            }
            StatusVarKey::UpdatedDbNames => {
                let mut read = self.value;
                let count = read.read_u8().map_err(|_| self.value)? as usize;
                let mut names = Vec::with_capacity(count);

                for _ in 0..count {
                    let index = read.iter().position(|x| *x == 0).ok_or(self.value)?;
                    names.push(RawText(&read[..index]));
                    read = &read[index..];
                }

                Ok(StatusVarVal::UpdatedDbNames(names))
            }
            StatusVarKey::Microseconds => {
                let mut read = self.value;
                let val = read.read_u32::<LittleEndian>().map_err(|_| self.value)?;
                Ok(StatusVarVal::Microseconds(val))
            }
            StatusVarKey::CommitTs => Ok(StatusVarVal::CommitTs(self.value)),
            StatusVarKey::CommitTs2 => Ok(StatusVarVal::CommitTs2(self.value)),
            StatusVarKey::ExplicitDefaultsForTimestamp => {
                let mut read = self.value;
                let val = read.read_u8().map_err(|_| self.value)?;
                Ok(StatusVarVal::ExplicitDefaultsForTimestamp(val != 0))
            }
            StatusVarKey::DdlLoggedWithXid => {
                let mut read = self.value;
                let val = read.read_u64::<LittleEndian>().map_err(|_| self.value)?;
                Ok(StatusVarVal::DdlLoggedWithXid(val))
            }
            StatusVarKey::DefaultCollationForUtf8mb4 => {
                let mut read = self.value;
                let val = read.read_u16::<LittleEndian>().map_err(|_| self.value)?;
                Ok(StatusVarVal::DefaultCollationForUtf8mb4(val))
            }
            StatusVarKey::SqlRequirePrimaryKey => {
                let mut read = self.value;
                let val = read.read_u8().map_err(|_| self.value)?;
                Ok(StatusVarVal::SqlRequirePrimaryKey(val))
            }
            StatusVarKey::DefaultTableEncryption => {
                let mut read = self.value;
                let val = read.read_u8().map_err(|_| self.value)?;
                Ok(StatusVarVal::DefaultTableEncryption(val))
            }
        }
    }
}

impl fmt::Debug for StatusVar<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StatusVar")
            .field("key", &self.key)
            .field("value", &self.get_value())
            .finish()
    }
}

/// Status variables of a QueryEvent.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct StatusVars(pub Vec<u8>);

impl StatusVars {
    /// Returns an iterator over QueryEvent status variables.
    pub fn iter(&self) -> StatusVarsIterator<'_> {
        StatusVarsIterator::new(&self.0)
    }

    /// Returns raw value of a status variable by key.
    pub fn get_status_var(&self, needle: StatusVarKey) -> Option<StatusVar> {
        self.iter()
            .find_map(|var| if var.key == needle { Some(var) } else { None })
    }
}

impl fmt::Debug for StatusVars {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.iter().fmt(f)
    }
}

/// Iterator over status vars of a `QueryEvent`.
///
/// It will stop iteration if vars can't be parsed.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct StatusVarsIterator<'a> {
    pos: usize,
    status_vars: &'a [u8],
}

impl<'a> StatusVarsIterator<'a> {
    /// Creates new instance.
    pub fn new(status_vars: &'a [u8]) -> StatusVarsIterator<'a> {
        Self {
            pos: 0,
            status_vars,
        }
    }
}

impl fmt::Debug for StatusVarsIterator<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.clone()).finish()
    }
}

impl<'a> Iterator for StatusVarsIterator<'a> {
    type Item = StatusVar<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let key = *self.status_vars.get(self.pos)?;
        let key = StatusVarKey::try_from(key).ok()?;
        self.pos += 1;

        macro_rules! get_fixed {
            ($len:expr) => {{
                self.pos += $len;
                self.status_vars.get((self.pos - $len)..self.pos)?
            }};
        }

        macro_rules! get_var {
            ($suffix_len:expr) => {{
                let len = *self.status_vars.get(self.pos)? as usize;
                get_fixed!(1 + len + $suffix_len)
            }};
        }

        let value = match key {
            StatusVarKey::Flags2 => get_fixed!(4),
            StatusVarKey::SqlMode => get_fixed!(8),
            StatusVarKey::Catalog => get_var!(1),
            StatusVarKey::AutoIncrement => get_fixed!(4),
            StatusVarKey::Charset => get_fixed!(6),
            StatusVarKey::TimeZone => get_var!(0),
            StatusVarKey::CatalogNz => get_var!(0),
            StatusVarKey::LcTimeNames => get_fixed!(2),
            StatusVarKey::CharsetDatabase => get_fixed!(2),
            StatusVarKey::TableMapForUpdate => get_fixed!(8),
            StatusVarKey::MasterDataWritten => get_fixed!(4),
            StatusVarKey::Invoker => {
                let user_len = *self.status_vars.get(self.pos)? as usize;
                let host_len = *self.status_vars.get(self.pos + 1 + user_len)? as usize;
                get_fixed!(1 + user_len + 1 + host_len)
            }
            StatusVarKey::UpdatedDbNames => {
                let mut total = 1;
                let count = *self.status_vars.get(self.pos)? as usize;
                for _ in 0..count {
                    while *self.status_vars.get(self.pos + total)? != 0x00 {
                        total += 1;
                    }
                    total += 1;
                }
                get_fixed!(total)
            }
            StatusVarKey::Microseconds => get_fixed!(3),
            StatusVarKey::CommitTs => get_fixed!(0),
            StatusVarKey::CommitTs2 => get_fixed!(0),
            StatusVarKey::ExplicitDefaultsForTimestamp => get_fixed!(1),
            StatusVarKey::DdlLoggedWithXid => get_fixed!(8),
            StatusVarKey::DefaultCollationForUtf8mb4 => get_fixed!(2),
            StatusVarKey::SqlRequirePrimaryKey => get_fixed!(1),
            StatusVarKey::DefaultTableEncryption => get_fixed!(1),
        };

        Some(StatusVar { key, value })
    }
}

bitflags! {
    /// Semi-sync binlog flags.
    pub struct SemiSyncFlags: u8 {
        // If the SEMI_SYNC_ACK_REQ flag is set the master waits for a Semi Sync ACK packet
        // from the slave before it sends the next event.
        const SEMI_SYNC_ACK_REQ = 0x01;
    }
}

/// Begin load query event.
///
/// Used for LOAD DATA INFILE statements as of MySQL 5.0.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct BeginLoadQueryEvent {
    pub file_id: u32,
    pub block_data: Vec<u8>,
}

impl BinlogStruct for BeginLoadQueryEvent {
    const EVENT_TYPE: Option<EventType> = Some(EventType::BEGIN_LOAD_QUERY_EVENT);

    fn read<T: Read>(
        event_size: usize,
        fde: &FormatDescriptionEvent,
        mut input: T,
    ) -> io::Result<Self> {
        let version = fde.binlog_version.get().unwrap_or(BinlogVersion::Version4);
        let mut input = input.limit(S(event_size) - S(BinlogEventHeader::len(version)));

        let file_id = input.read_u32::<LittleEndian>()?;

        let mut block_data = vec![0_u8; input.get_limit()];
        input.read_exact(&mut block_data)?;

        Ok(Self {
            file_id,
            block_data,
        })
    }

    fn write<T: Write>(&self, version: BinlogVersion, mut output: T) -> io::Result<()> {
        let mut output = output.limit(S(self.len(version)));

        output.write_u32::<LittleEndian>(self.file_id)?;
        output.write_all(&self.block_data)?;

        Ok(())
    }

    /// Returns length of this load event in bytes.
    fn len(&self, version: BinlogVersion) -> usize {
        let mut len = S(0);

        len += S(4);
        len += S(self.block_data.len());

        min(len.0, u32::MAX as usize - BinlogEventHeader::len(version))
    }
}

/// Variants of this enum describe how LOAD DATA handles duplicates.
#[repr(u8)]
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum LoadDuplicateHandling {
    LOAD_DUP_ERROR = 0,
    LOAD_DUP_IGNORE,
    LOAD_DUP_REPLACE,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
#[error("Unknown duplicate handling variant {}", _0)]
#[repr(transparent)]
pub struct UnknownDuplicateHandling(pub u8);

impl From<UnknownDuplicateHandling> for u8 {
    fn from(x: UnknownDuplicateHandling) -> Self {
        x.0
    }
}

impl TryFrom<u8> for LoadDuplicateHandling {
    type Error = UnknownDuplicateHandling;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::LOAD_DUP_ERROR),
            1 => Ok(Self::LOAD_DUP_IGNORE),
            2 => Ok(Self::LOAD_DUP_REPLACE),
            x => Err(UnknownDuplicateHandling(x)),
        }
    }
}

/// Execute load query event.
///
/// Used for LOAD DATA INFILE statements as of MySQL 5.0.
///
/// It similar to Query_log_event but before executing the query it substitutes original filename
/// in LOAD DATA query with name of temporary file.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct ExecuteLoadQueryEvent {
    // post-header
    pub thread_id: u32,
    pub execution_time: u32,
    pub error_code: u16,

    pub status_vars: Vec<u8>,
    pub schema: RawText,
    pub query: RawText,

    // payload
    /// File_id of a temporary file.
    pub file_id: u32,
    /// Pointer to the part of the query that should be substituted.
    pub start_pos: u32,
    /// Pointer to the end of this part of query
    pub end_pos: u32,
    /// How to handle duplicates.
    pub dup_handling: RawField<u8, UnknownDuplicateHandling, LoadDuplicateHandling>,
}

impl BinlogStruct for ExecuteLoadQueryEvent {
    const EVENT_TYPE: Option<EventType> = Some(EventType::EXECUTE_LOAD_QUERY_EVENT);

    fn read<T: Read>(
        event_size: usize,
        fde: &FormatDescriptionEvent,
        mut input: T,
    ) -> io::Result<Self>
    where
        Self: Sized,
    {
        let version = fde.binlog_version.get().unwrap_or(BinlogVersion::Version4);
        let mut input = input.limit(S(event_size) - S(BinlogEventHeader::len(version)));

        let thread_id = input.read_u32::<LittleEndian>()?;
        let execution_time = input.read_u32::<LittleEndian>()?;
        let schema_len = input.read_u8()? as usize;
        let error_code = input.read_u16::<LittleEndian>()?;
        let status_vars_len = input.read_u16::<LittleEndian>()? as usize;
        let file_id = input.read_u32::<LittleEndian>()?;
        let start_pos = input.read_u32::<LittleEndian>()?;
        let end_pos = input.read_u32::<LittleEndian>()?;
        let dup_handling = input.read_u8()?;

        let mut status_vars = vec![0_u8; status_vars_len];
        input.read_exact(&mut status_vars)?;

        let mut schema = vec![0_u8; schema_len];
        input.read_exact(&mut schema)?;
        input.read_u8()?;

        let mut query = vec![0_u8; input.get_limit()];
        input.read_exact(&mut query)?;

        Ok(Self {
            thread_id,
            execution_time,
            error_code,
            status_vars,
            schema: RawText(schema),
            file_id,
            start_pos,
            end_pos,
            dup_handling: RawField::new(dup_handling),
            query: RawText(query),
        })
    }

    fn write<T: Write>(&self, version: BinlogVersion, mut output: T) -> io::Result<()> {
        let mut output = output.limit(S(self.len(version)));

        output.write_u32::<LittleEndian>(self.thread_id)?;
        output.write_u32::<LittleEndian>(self.execution_time)?;
        output.write_u8(min(self.schema.0.len(), u8::MAX as usize) as u8)?;
        output.write_u16::<LittleEndian>(self.error_code)?;
        output.write_u16::<LittleEndian>(min(self.status_vars.len(), u16::MAX as usize) as u16)?;
        output.write_u32::<LittleEndian>(self.file_id)?;
        output.write_u32::<LittleEndian>(self.start_pos)?;
        output.write_u32::<LittleEndian>(self.end_pos)?;
        output.write_u8(self.dup_handling.0)?;
        output
            .limit(S(u16::MAX as usize))
            .write_all(&self.status_vars)?;
        output
            .limit(S(u8::MAX as usize))
            .write_all(&self.schema.0)?;
        output.write_u8(0)?;
        output.write_all(&self.query.0)?;

        Ok(())
    }

    fn len(&self, version: BinlogVersion) -> usize {
        let mut len = S(0);

        len += S(4); // thread_id
        len += S(4); // query_exec_time
        len += S(1); // db_len
        len += S(2); // error_code
        len += S(2); // status_vars_len
        len += S(4); // file_id
        len += S(4); // start_pos
        len += S(4); // end_pos
        len += S(1); // dup_handling_flags
        len += S(min(self.status_vars.len(), u16::MAX as usize - 13)); // status_vars
        len += S(min(self.schema.0.len(), u8::MAX as usize)); // db_len
        len += S(1); // null-byte
        len += S(self.query.0.len());

        min(len.0, u32::MAX as usize - BinlogEventHeader::len(version))
    }
}

/// Rand event.
///
/// Logs random seed used by the next `RAND()`, and by `PASSWORD()` in 4.1.0. 4.1.1 does not need
/// it (it's repeatable again) so this event needn't be written in 4.1.1 for `PASSWORD()`
/// (but the fact that it is written is just a waste, it does not cause bugs).
///
/// The state of the random number generation consists of 128 bits, which are stored internally
/// as two 64-bit numbers.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct RandEvent {
    pub seed1: u64,
    pub seed2: u64,
}

impl BinlogStruct for RandEvent {
    const EVENT_TYPE: Option<EventType> = Some(EventType::RAND_EVENT);

    fn read<T: Read>(
        event_size: usize,
        fde: &FormatDescriptionEvent,
        mut input: T,
    ) -> io::Result<Self>
    where
        Self: Sized,
    {
        let version = fde.binlog_version.get().unwrap_or(BinlogVersion::Version4);
        let mut input = input.limit(S(event_size) - S(BinlogEventHeader::len(version)));

        let seed1 = input.read_u64::<LittleEndian>()?;
        let seed2 = input.read_u64::<LittleEndian>()?;

        if input.get_limit() > 0 {
            return Err(Error::new(Other, "bytes remaining on stream"));
        }

        Ok(Self { seed1, seed2 })
    }

    fn write<T: Write>(&self, _version: BinlogVersion, mut output: T) -> io::Result<()> {
        output.write_u64::<LittleEndian>(self.seed1)?;
        output.write_u64::<LittleEndian>(self.seed2)?;
        Ok(())
    }

    fn len(&self, _version: BinlogVersion) -> usize {
        8
    }
}

/// Xid event.
///
/// Generated for a commit of a transaction that modifies one or more tables of an XA-capable
/// storage engine.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct XidEvent {
    pub xid: u64,
}

impl BinlogStruct for XidEvent {
    const EVENT_TYPE: Option<EventType> = Some(EventType::XID_EVENT);

    fn read<T: Read>(
        event_size: usize,
        fde: &FormatDescriptionEvent,
        mut input: T,
    ) -> io::Result<Self>
    where
        Self: Sized,
    {
        let version = fde.binlog_version.get().unwrap_or(BinlogVersion::Version4);
        let mut input = input.limit(S(event_size) - S(BinlogEventHeader::len(version)));

        let post_header_len = fde.get_event_type_header_length(Self::EVENT_TYPE.unwrap());

        for _ in 0..post_header_len {
            input.read_u8()?;
        }

        let xid = input.read_u64::<LittleEndian>()?;

        if input.get_limit() > 0 {
            return Err(Error::new(Other, "bytes remaining on stream"));
        }

        Ok(Self { xid })
    }

    fn write<T: Write>(&self, _version: BinlogVersion, mut output: T) -> io::Result<()> {
        output.write_u64::<LittleEndian>(self.xid)
    }

    fn len(&self, _version: BinlogVersion) -> usize {
        8
    }
}

/// Type of an `InvarEvent`.
#[repr(u8)]
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum IntvarEventType {
    INVALID_INT_EVENT,
    /// Indicates the value to use for the `LAST_INSERT_ID()` function in the next statement.
    LAST_INSERT_ID_EVENT,
    /// Indicates the value to use for an `AUTO_INCREMENT` column in the next statement.
    INSERT_ID_EVENT,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
#[error("Unknown intvar event type {}", _0)]
#[repr(transparent)]
pub struct UnknownIntvarEventType(pub u8);

impl From<UnknownIntvarEventType> for u8 {
    fn from(x: UnknownIntvarEventType) -> Self {
        x.0
    }
}

impl TryFrom<u8> for IntvarEventType {
    type Error = UnknownIntvarEventType;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::INVALID_INT_EVENT),
            1 => Ok(Self::LAST_INSERT_ID_EVENT),
            2 => Ok(Self::INSERT_ID_EVENT),
            x => Err(UnknownIntvarEventType(x)),
        }
    }
}

/// Integer based session-variables event.
///
/// Written every time a statement uses an AUTO_INCREMENT column or the LAST_INSERT_ID() function;
/// precedes other events for the statement. This is written only before a QUERY_EVENT
/// and is not used with row-based logging. An INTVAR_EVENT is written with a "subtype"
/// in the event data part.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct IntvarEvent {
    /// Subtype of this event.
    pub subtype: RawField<u8, UnknownIntvarEventType, IntvarEventType>,
    pub value: u64,
}

impl BinlogStruct for IntvarEvent {
    const EVENT_TYPE: Option<EventType> = Some(EventType::INTVAR_EVENT);

    fn read<T: Read>(
        event_size: usize,
        fde: &FormatDescriptionEvent,
        mut input: T,
    ) -> io::Result<Self>
    where
        Self: Sized,
    {
        let version = fde.binlog_version.get().unwrap_or(BinlogVersion::Version4);
        let mut input = input.limit(S(event_size) - S(BinlogEventHeader::len(version)));

        let post_header_len = fde.get_event_type_header_length(Self::EVENT_TYPE.unwrap());

        for _ in 0..post_header_len {
            input.read_u8()?;
        }

        let subtype = input.read_u8()?;
        let value = input.read_u64::<LittleEndian>()?;

        if input.get_limit() > 0 {
            return Err(Error::new(Other, "bytes remaining on stream"));
        }

        Ok(Self {
            subtype: RawField::new(subtype),
            value,
        })
    }

    fn write<T: Write>(&self, _version: BinlogVersion, mut output: T) -> io::Result<()> {
        output.write_u8(self.subtype.0)?;
        output.write_u64::<LittleEndian>(self.value)?;
        Ok(())
    }

    fn len(&self, _version: BinlogVersion) -> usize {
        9
    }
}

my_bitflags! {
    UserVarFlags, u8,

    /// Flags of a user variable.
    pub struct UserVarFlags: u8 {
        const UNSIGNED = 0x01;
    }
}

/// User variable event.
///
/// Written every time a statement uses a user variable; precedes other events for the statement.
/// Indicates the value to use for the user variable in the next statement.
/// This is written only before a `QUERY_EVENT` and is not used with row-based logging.
///
/// # Notes on `BinlogEvent` implementation
///
/// * it won't try to read/write anything except `name` and `is_null` if `is_null` is `true`
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct UserVarEvent {
    /// User variable name.
    pub name: RawText,
    /// `true` if value is `NULL`.
    pub is_null: bool,
    /// Type of a value.
    pub value_type: RawField<i8, UnknownItemResultType, ItemResult>,
    /// Character set of a value. Will be `0` if `is_null` is `true`.
    pub charset: u32,
    /// Value of a user variable. Will be empty if `is_null` is `true`.
    pub value: Vec<u8>,
    /// Flags of a user variable. Will be `0` if `is_null` is `true`.
    ///
    /// This field contains raw value. Use `Self::get_flags` to parse it.
    pub flags: RawFlags<UserVarFlags>,
}

impl BinlogStruct for UserVarEvent {
    const EVENT_TYPE: Option<EventType> = Some(EventType::USER_VAR_EVENT);

    fn read<T: Read>(
        event_size: usize,
        fde: &FormatDescriptionEvent,
        mut input: T,
    ) -> io::Result<Self>
    where
        Self: Sized,
    {
        let version = fde.binlog_version.get().unwrap_or(BinlogVersion::Version4);
        let mut input = input.limit(S(event_size) - S(BinlogEventHeader::len(version)));

        let name_len = input.read_u32::<LittleEndian>()? as usize;
        let mut name = vec![0_u8; name_len];
        input.read_exact(&mut name)?;
        let is_null = input.read_u8()? != 0;

        if is_null {
            return Ok(Self {
                name: RawText(name),
                is_null,
                value_type: RawField::new(ItemResult::STRING_RESULT as i8),
                charset: 63,
                value: Vec::new(),
                flags: RawFlags(UserVarFlags::empty().bits()),
            });
        }

        let value_type = input.read_i8()?;
        let charset = input.read_u32::<LittleEndian>()?;
        let value_len = input.read_u32::<LittleEndian>()? as usize;
        let mut value = vec![0_u8; value_len];
        input.read_exact(&mut value)?;

        // Old servers may not pack flags here.
        let flags = if input.get_limit() > 0 {
            input.read_u8()?
        } else {
            0
        };

        if input.get_limit() > 0 {
            return Err(Error::new(Other, "bytes remaining on stream"));
        }

        Ok(Self {
            name: RawText(name),
            is_null,
            value_type: RawField::new(value_type),
            charset,
            value,
            flags: RawFlags(flags),
        })
    }

    fn write<T: Write>(&self, version: BinlogVersion, mut output: T) -> io::Result<()> {
        let mut output = output.limit(S(self.len(version)));

        output.write_u32::<LittleEndian>(self.name.0.len() as u32)?;
        output.write_all(&self.name.0)?;
        output.write_u8(self.is_null as u8)?;
        if !self.is_null {
            output.write_i8(self.value_type.0)?;
            output.write_u32::<LittleEndian>(self.charset)?;
            output.write_u32::<LittleEndian>(self.value.len() as u32)?;
            output.write_all(&self.value)?;
            output.write_u8(self.flags.0)?;
        }
        Ok(())
    }

    fn len(&self, version: BinlogVersion) -> usize {
        let mut len = S(0);

        len += S(4);
        len += S(min(self.name.0.len(), u32::MAX as usize));
        len += S(1);

        if !self.is_null {
            len += S(1);
            len += S(4);
            len += S(4);
            len += S(min(self.value.len(), u32::MAX as usize));
            len += S(1);
        }

        min(len.0, u32::MAX as usize - BinlogEventHeader::len(version))
    }
}

/// Type of an incident event.
#[repr(u16)]
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum IncidentType {
    /// No incident.
    INCIDENT_NONE = 0,
    /// There are possibly lost events in the replication stream.
    INCIDENT_LOST_EVENTS = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
#[error("Unknown item incident type {}", _0)]
#[repr(transparent)]
pub struct UnknownIncidentType(pub u16);

impl From<UnknownIncidentType> for u16 {
    fn from(x: UnknownIncidentType) -> Self {
        x.0
    }
}

impl TryFrom<u16> for IncidentType {
    type Error = UnknownIncidentType;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::INCIDENT_NONE),
            1 => Ok(Self::INCIDENT_LOST_EVENTS),
            x => Err(UnknownIncidentType(x)),
        }
    }
}

/// Used to log an out of the ordinary event that occurred on the master.
///
/// It notifies the slave that something happened on the master that might cause data
/// to be in an inconsistent state.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct IncidentEvent {
    pub incident_type: RawField<u16, UnknownIncidentType, IncidentType>,
    pub message: RawText,
}

impl BinlogStruct for IncidentEvent {
    const EVENT_TYPE: Option<EventType> = Some(EventType::INCIDENT_EVENT);

    fn read<T: Read>(
        event_size: usize,
        fde: &FormatDescriptionEvent,
        mut input: T,
    ) -> io::Result<Self>
    where
        Self: Sized,
    {
        let version = fde.binlog_version.get().unwrap_or(BinlogVersion::Version4);
        let mut input = input.limit(S(event_size) - S(BinlogEventHeader::len(version)));

        let incident_type = input.read_u16::<LittleEndian>()?;
        let message_len = input.read_u8()? as usize;
        let mut message = vec![0_u8; message_len];
        input.read_exact(&mut message)?;

        if input.get_limit() > 0 {
            return Err(Error::new(Other, "bytes remaining on stream"));
        }

        Ok(Self {
            incident_type: RawField::new(incident_type),
            message: RawText(message),
        })
    }

    fn write<T: Write>(&self, version: BinlogVersion, mut output: T) -> io::Result<()> {
        let mut output = output.limit(S(self.len(version)));
        output.write_u16::<LittleEndian>(self.incident_type.0)?;
        output.write_u8(min(self.message.0.len(), u8::MAX as usize) as u8)?;
        output
            .limit(S(u8::MAX as usize))
            .write_all(&self.message.0)?;
        Ok(())
    }

    fn len(&self, version: BinlogVersion) -> usize {
        let mut len = S(0);

        len += S(2);
        len += S(1);
        len += S(min(self.message.0.len(), u8::MAX as usize));

        min(len.0, u32::MAX as usize - BinlogEventHeader::len(version))
    }
}

impl ColumnType {
    /// Returns type-specific metadata length for this column type.
    fn get_metadata_len(&self) -> usize {
        match self {
            Self::MYSQL_TYPE_STRING => 2,
            Self::MYSQL_TYPE_VAR_STRING => 2,
            Self::MYSQL_TYPE_VARCHAR => 2,
            Self::MYSQL_TYPE_BLOB => 1,
            Self::MYSQL_TYPE_DECIMAL => 2,
            Self::MYSQL_TYPE_NEWDECIMAL => 2,
            Self::MYSQL_TYPE_DOUBLE => 1,
            Self::MYSQL_TYPE_FLOAT => 1,
            Self::MYSQL_TYPE_SET | Self::MYSQL_TYPE_ENUM => 2,
            _ => 0,
        }
    }
}

/// Table map event.
///
/// In row-based mode, every row operation event is preceded by a Table_map_event which maps
/// a table definition to a number.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct TableMapEvent {
    // post-header
    /// The number that identifies the table.
    ///
    /// It's 6 bytes long, so valid range is [0, 1<<48).
    pub table_id: u64,
    /// Reserved for future use; currently always 0.
    pub flags: u16,

    // payload
    /// The name of the database in which the table resides.
    ///
    /// Length must be <= 64 bytes.
    pub database_name: RawText,
    /// The name of the table.
    ///
    /// Length must be <= 64 bytes.
    pub table_name: RawText,
    /// The type of each column in the table, listed from left to right.
    pub columns_type: RawSeq<u8, UnknownColumnType, ColumnType>,
    /// For each column from left to right, a chunk of data who's length and semantics depends
    /// on the type of the column.
    pub columns_metadata: Vec<u8>,
    /// For each column, a bit indicating whether data in the column can be NULL or not.
    ///
    /// The number of bytes needed for this is int((column_count + 7) / 8).
    /// The flag for the first column from the left is in the least-significant bit
    /// of the first byte, the second is in the second least significant bit of the first byte,
    /// the ninth is in the least significant bit of the second byte, and so on.
    pub null_bitmask: BitVec<Lsb0, u8>,
    /// Optional metadata.
    pub optional_metadata: Vec<u8>,
}

impl TableMapEvent {
    /// Returns columns count in this event.
    pub fn get_columns_count(&self) -> usize {
        self.columns_type.0.len()
    }

    /// Returns metadata for the given column.
    ///
    /// Returns `None` if column index is out of bounds or if offset couldn't be calculated
    /// (e.g. because of unknown column type between `0` and `col_idx`).
    pub fn get_column_metadata(&self, col_idx: usize) -> Option<&[u8]> {
        let col_type = self.columns_type.get(col_idx)?.ok()?;
        let metadata_len = col_type.get_metadata_len();

        let mut offset = 0;

        for _ in 0..col_idx {
            let ty = self.columns_type.get(col_idx)?.ok()?;
            offset += ty.get_metadata_len();
        }

        self.columns_metadata.get(offset..(offset + metadata_len))
    }
}

impl BinlogStruct for TableMapEvent {
    const EVENT_TYPE: Option<EventType> = Some(EventType::TABLE_MAP_EVENT);

    fn read<T: Read>(
        event_size: usize,
        fde: &FormatDescriptionEvent,
        mut input: T,
    ) -> io::Result<Self>
    where
        Self: Sized,
    {
        let version = fde.binlog_version.get().unwrap_or(BinlogVersion::Version4);
        let mut input = input.limit(S(event_size) - S(BinlogEventHeader::len(version)));

        let table_id = if 6 == fde.get_event_type_header_length(Self::EVENT_TYPE.unwrap()) {
            input.read_u32::<LittleEndian>()? as u64
        } else {
            input.read_u48::<LittleEndian>()?
        };

        let flags = input.read_u16::<LittleEndian>()?;

        let database_name_len = input.read_u8()? as usize;
        let mut database_name = vec![0_u8; database_name_len];
        input.read_exact(&mut database_name)?;
        input.read_u8()?; // skip null

        let table_name_len = input.read_u8()? as usize;
        let mut table_name = vec![0_u8; table_name_len];
        input.read_exact(&mut table_name)?;
        input.read_u8()?; // skip null

        let columns_count = input.read_lenenc_int()?;
        let mut columns_type = vec![0_u8; columns_count as usize];
        input.read_exact(&mut columns_type)?;

        let metadata_len = input.read_lenenc_int()? as usize;
        let mut columns_metadata = vec![0_u8; metadata_len];
        input.read_exact(&mut columns_metadata)?;

        let bitmask_len = (columns_count + 7) / 8;
        let mut null_bitmask = vec![0_u8; bitmask_len as usize];
        input.read_exact(&mut null_bitmask)?;

        let mut optional_metadata = vec![0_u8; input.get_limit()];
        input.read_exact(&mut optional_metadata)?;

        let mut null_bitmask = BitVec::from_vec(null_bitmask);
        null_bitmask.truncate(columns_count as usize);

        Ok(Self {
            table_id,
            flags,
            database_name: RawText(database_name),
            table_name: RawText(table_name),
            columns_type: RawSeq::new(columns_type),
            columns_metadata,
            null_bitmask,
            optional_metadata,
        })
    }

    fn write<T: Write>(&self, version: BinlogVersion, mut output: T) -> io::Result<()> {
        let mut output = output.limit(S(self.len(version)));

        output.write_u48::<LittleEndian>(self.table_id)?;
        output.write_u16::<LittleEndian>(self.flags)?;
        output.write_u8(min(self.database_name.0.len(), u8::MAX as usize) as u8)?;
        output
            .limit(S(u8::MAX as usize))
            .write_all(&self.database_name.0)?;
        output.write_u8(0)?;
        output.write_u8(min(self.table_name.0.len(), u8::MAX as usize) as u8)?;
        output
            .limit(S(u8::MAX as usize))
            .write_all(&self.table_name.0)?;
        output.write_u8(0)?;
        output.write_lenenc_int(self.get_columns_count() as u64)?;
        output.write_all(&self.columns_type.0)?;
        output.write_lenenc_str(&self.columns_metadata)?;
        output.write_all(self.null_bitmask.as_raw_slice())?;
        output.write_all(&self.optional_metadata)?;

        Ok(())
    }

    fn len(&self, version: BinlogVersion) -> usize {
        let mut len = S(0);

        len += S(6);
        len += S(2);
        len += S(1);
        len += S(min(self.database_name.0.len(), u8::MAX as usize));
        len += S(1);
        len += S(1);
        len += S(min(self.table_name.0.len(), u8::MAX as usize));
        len += S(1);
        len += S(crate::misc::lenenc_int_len(self.get_columns_count() as u64) as usize);
        len += S(self.get_columns_count());
        len += S(crate::misc::lenenc_str_len(&self.columns_metadata) as usize);
        len += S((self.get_columns_count() + 8) / 7);
        len += S(self.optional_metadata.len());

        min(len.0, u32::MAX as usize - BinlogEventHeader::len(version))
    }
}
/// Query that caused the following `ROWS_EVENT`.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct RowsQueryEvent {
    pub query: RawText,
}

impl BinlogStruct for RowsQueryEvent {
    const EVENT_TYPE: Option<EventType> = Some(EventType::ROWS_QUERY_EVENT);

    fn read<T: Read>(
        event_size: usize,
        fde: &FormatDescriptionEvent,
        mut input: T,
    ) -> io::Result<Self>
    where
        Self: Sized,
    {
        let version = fde.binlog_version.get().unwrap_or(BinlogVersion::Version4);
        let mut input = input.limit(S(event_size) - S(BinlogEventHeader::len(version)));

        input.read_u8()?; // ignore length
        let mut query = vec![0_u8; input.get_limit()];
        input.read_exact(&mut query)?;

        Ok(Self {
            query: RawText(query),
        })
    }

    fn write<T: Write>(&self, version: BinlogVersion, mut output: T) -> io::Result<()> {
        let mut output = output.limit(S(self.len(version)));

        output.write_u8(min(self.query.0.len(), u8::MAX as usize) as u8)?;
        output.write_all(&self.query.0)?;

        Ok(())
    }

    fn len(&self, version: BinlogVersion) -> usize {
        let mut len = S(0);

        len += S(1);
        len += S(self.query.0.len());

        min(len.0, u32::MAX as usize - BinlogEventHeader::len(version))
    }
}

my_bitflags! {
    RowsEventFlags, u16,

    /// Rows event flags.
    pub struct RowsEventFlags: u16 {
        /// Last event of a statement.
        const STMT_END = 0x0001;
        /// No foreign key checks.
        const NO_FOREIGN_KEY_CHECKS   = 0x0002;
        /// No unique key checks.
        const RELAXED_UNIQUE_CHECKS  = 0x0004;
        /// Indicates that rows in this event are complete,
        /// that is contain values for all columns of the table.
        const COMPLETE_ROWS = 0x0008;
    }
}

/// Common base structure for all row-containing binary log events.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct RowsEvent {
    /// Table identifier.
    ///
    /// If the table id is `0x00ffffff` it is a dummy event that should have
    /// the end of statement flag set that declares that all table maps can be freed.
    /// Otherwise it refers to a table defined by `TABLE_MAP_EVENT`.
    pub table_id: u64,
    /// Raw rows event flags (see `RowsEventFalgs`).
    pub flags: RawFlags<RowsEventFlags>,
    /// Raw extra data.
    pub extra_data: Vec<u8>,
    /// Number of columns.
    pub num_columns: u64,
    /// For DELETE and UPDATE only. Bit-field indicating whether each column is used one bit
    /// per column.
    ///
    /// Will be empty for WRITE events.
    pub columns_before_image: Option<BitVec<Lsb0, u8>>,
    /// For WRITE and UPDATE only. Bit-field indicating whether each column is used
    /// in the `UPDATE_ROWS_EVENT` and `WRITE_ROWS_EVENT` after-image; one bit per column.
    ///
    /// Will be empty for DELETE events.
    pub columns_after_image: Option<BitVec<Lsb0, u8>>,
    /// A sequence of zero or more rows. The end is determined by the size of the event.
    ///
    /// Each row has the following format:
    ///
    /// *   A Bit-field indicating whether each field in the row is NULL. Only columns that
    ///     are "used" according to the second field in the variable data part are listed here.
    ///     If the second field in the variable data part has N one-bits, the amount of storage
    ///     required for this field is INT((N + 7) / 8) bytes.
    /// *   The row-image, containing values of all table fields. This only lists table fields
    ///     that are used (according to the second field of the variable data part) and non-NULL
    ///     (according to the previous field). In other words, the number of values listed here
    ///     is equal to the number of zero bits in the previous field. (not counting padding
    ///     bits in the last byte).
    pub rows_data: Vec<u8>,
}

impl RowsEvent {
    /// Reads an event from the given stream.
    ///
    /// This function will be used in `BinlogStruct` implementations for derived events.
    pub fn read<T: Read>(
        event_type: EventType,
        event_size: usize,
        fde: &FormatDescriptionEvent,
        version: BinlogVersion,
        mut input: T,
    ) -> io::Result<Self> {
        let mut input = input.limit(S(event_size) - S(BinlogEventHeader::len(version)));
        let post_header_len = fde.get_event_type_header_length(event_type);

        let is_delete_event = event_type == EventType::DELETE_ROWS_EVENT
            || event_type == EventType::DELETE_ROWS_EVENT_V1;

        let is_update_event = event_type == EventType::UPDATE_ROWS_EVENT
            || event_type == EventType::UPDATE_ROWS_EVENT_V1
            || event_type == EventType::PARTIAL_UPDATE_ROWS_EVENT;

        let table_id = if post_header_len == 6 {
            input.read_u32::<LittleEndian>()? as u64
        } else {
            input.read_u48::<LittleEndian>()?
        };

        let flags = input.read_u16::<LittleEndian>()?;

        let extra_data =
            if post_header_len == fde.get_event_type_header_length(EventType::WRITE_ROWS_EVENT) {
                // variable-length post header containing extra data
                let extra_data_len = input.read_u16::<LittleEndian>()? as usize;
                let mut extra_data = vec![0_u8; extra_data_len.saturating_sub(2)];
                input.read_exact(&mut extra_data)?;
                extra_data
            } else {
                Vec::new()
            };

        let num_columns = input.read_lenenc_int()?;
        let bitmap_len = (num_columns as usize + 7) / 8;

        let mut columns_image_1 = vec![0_u8; bitmap_len];
        input.read_exact(&mut columns_image_1)?;

        let columns_image_2 = if is_update_event {
            let mut columns_image_2 = vec![0_u8; bitmap_len];
            input.read_exact(&mut columns_image_2)?;
            Some(columns_image_2)
        } else {
            None
        };

        let mut rows_data = vec![0_u8; input.get_limit()];
        input.read_exact(&mut rows_data)?;

        let (columns_before_image, columns_after_image) = if is_update_event {
            (Some(columns_image_1), columns_image_2)
        } else if is_delete_event {
            (Some(columns_image_1), None)
        } else {
            (None, Some(columns_image_1))
        };

        Ok(Self {
            table_id,
            flags: RawFlags(flags),
            extra_data,
            num_columns,
            columns_before_image: columns_before_image.map(|val| {
                let mut bitvec = BitVec::from_vec(val);
                bitvec.truncate(num_columns as usize);
                bitvec
            }),
            columns_after_image: columns_after_image.map(|val| {
                let mut bitvec = BitVec::from_vec(val);
                bitvec.truncate(num_columns as usize);
                bitvec
            }),
            rows_data,
        })
    }

    /// Writes this event into the given stream.
    ///
    /// This function will be used in `BinlogStruct` implementations for derived events.
    pub fn write<T: Write>(&self, version: BinlogVersion, mut output: T) -> io::Result<()> {
        let mut output = output.limit(S(self.len(version)));

        output.write_u48::<LittleEndian>(self.table_id)?;
        output.write_u16::<LittleEndian>(self.flags.0)?;
        output.write_u16::<LittleEndian>(min(
            self.extra_data.len().saturating_add(2),
            u16::MAX as usize,
        ) as u16)?;
        output
            .limit(S(u16::MAX as usize - 2))
            .write_all(&self.extra_data)?;
        output.write_lenenc_int(self.num_columns)?;
        let bitmap_len = (self.num_columns as usize + 7) / 8;
        {
            let num_bitmaps = self.columns_before_image.is_some() as usize
                + self.columns_after_image.is_some() as usize;
            let mut output = output.limit(S(bitmap_len) * S(num_bitmaps));
            output.write_all(
                self.columns_before_image
                    .as_ref()
                    .map(|x| x.as_raw_slice())
                    .unwrap_or_default(),
            )?;
            output.write_all(
                self.columns_after_image
                    .as_ref()
                    .map(|x| x.as_raw_slice())
                    .unwrap_or_default(),
            )?;

            if output.get_limit() > 0 {
                return Err(Error::new(UnexpectedEof, "failed to fill whole buffer"));
            }
        }
        output.write_all(&self.rows_data)?;

        Ok(())
    }

    /// Returns length of this event in bytes.
    ///
    /// This function will be used in `BinlogStruct` implementations for derived events.
    pub fn len(&self, version: BinlogVersion) -> usize {
        let mut len = S(0);

        len += S(6); // table_id
        len += S(2); // flags
        len += S(2); // extra-data len
        len += S(min(self.extra_data.len(), u16::MAX as usize - 2)); // extra data
        len += S(crate::misc::lenenc_int_len(self.num_columns) as usize); // number of columns
        let bitmap_len = (self.num_columns as usize + 7) / 8;
        if self.columns_before_image.is_some() {
            len += S(bitmap_len); // columns present bitmap 1
        }
        if self.columns_after_image.is_some() {
            len += S(bitmap_len); // columns present bitmap 2
        }
        len += S(self.rows_data.len());

        min(len.0, u32::MAX as usize - BinlogEventHeader::len(version))
    }
}

/// Write rows event.
///
/// Used for row-based binary logging. Contains the row data to insert.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct WriteRowsEvent(pub RowsEvent);

impl BinlogStruct for WriteRowsEvent {
    const EVENT_TYPE: Option<EventType> = Some(EventType::WRITE_ROWS_EVENT);

    fn read<T: Read>(event_size: usize, fde: &FormatDescriptionEvent, input: T) -> io::Result<Self>
    where
        Self: Sized,
    {
        let version = fde.binlog_version.get().unwrap_or(BinlogVersion::Version4);
        Ok(Self(RowsEvent::read(
            Self::EVENT_TYPE.unwrap(),
            event_size,
            fde,
            version,
            input,
        )?))
    }

    fn write<T: Write>(&self, version: BinlogVersion, output: T) -> io::Result<()> {
        self.0.write(version, output)
    }

    fn len(&self, version: BinlogVersion) -> usize {
        self.0.len(version)
    }
}

/// Update rows event.
///
/// Used for row-based binary logging. Contains as much data as needed to identify
/// a row + the data to change.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct UpdateRowsEvent(pub RowsEvent);

impl BinlogStruct for UpdateRowsEvent {
    const EVENT_TYPE: Option<EventType> = Some(EventType::UPDATE_ROWS_EVENT);

    fn read<T: Read>(event_size: usize, fde: &FormatDescriptionEvent, input: T) -> io::Result<Self>
    where
        Self: Sized,
    {
        let version = fde.binlog_version.get().unwrap_or(BinlogVersion::Version4);
        Ok(Self(RowsEvent::read(
            Self::EVENT_TYPE.unwrap(),
            event_size,
            fde,
            version,
            input,
        )?))
    }

    fn write<T: Write>(&self, version: BinlogVersion, output: T) -> io::Result<()> {
        self.0.write(version, output)
    }

    fn len(&self, version: BinlogVersion) -> usize {
        self.0.len(version)
    }
}

/// Delete rows event.
///
/// Used for row-based binary logging. Contains as much data as needed to identify a row.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct DeleteRowsEvent(pub RowsEvent);

impl BinlogStruct for DeleteRowsEvent {
    const EVENT_TYPE: Option<EventType> = Some(EventType::DELETE_ROWS_EVENT);

    fn read<T: Read>(event_size: usize, fde: &FormatDescriptionEvent, input: T) -> io::Result<Self>
    where
        Self: Sized,
    {
        let version = fde.binlog_version.get().unwrap_or(BinlogVersion::Version4);
        Ok(Self(RowsEvent::read(
            Self::EVENT_TYPE.unwrap(),
            event_size,
            fde,
            version,
            input,
        )?))
    }

    fn write<T: Write>(&self, version: BinlogVersion, output: T) -> io::Result<()> {
        self.0.write(version, output)
    }

    fn len(&self, version: BinlogVersion) -> usize {
        self.0.len(version)
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    const BINLOG_FILE: &[u8] = &[
        0xfe, 0x62, 0x69, 0x6e, 0xfc, 0x35, 0xbb, 0x4a, 0x0f, 0x01, 0x00, 0x00, 0x00, 0x5e, 0x00,
        0x00, 0x00, 0x62, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x35, 0x2e, 0x30, 0x2e, 0x38,
        0x36, 0x2d, 0x64, 0x65, 0x62, 0x75, 0x67, 0x2d, 0x6c, 0x6f, 0x67, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0xfc, 0x35, 0xbb, 0x4a, 0x13, 0x38, 0x0d, 0x00, 0x08, 0x00, 0x12, 0x00, 0x04, 0x04, 0x04,
        0x04, 0x12, 0x00, 0x00, 0x4b, 0x00, 0x04, 0x1a, 0xfd, 0x35, 0xbb, 0x4a, 0x02, 0x01, 0x00,
        0x00, 0x00, 0x64, 0x00, 0x00, 0x00, 0xc6, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x1a, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00,
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x03, 0x73, 0x74, 0x64, 0x04,
        0x08, 0x00, 0x08, 0x00, 0x08, 0x00, 0x74, 0x65, 0x73, 0x74, 0x00, 0x63, 0x72, 0x65, 0x61,
        0x74, 0x65, 0x20, 0x74, 0x61, 0x62, 0x6c, 0x65, 0x20, 0x74, 0x31, 0x28, 0x61, 0x20, 0x69,
        0x6e, 0x74, 0x29, 0x20, 0x65, 0x6e, 0x67, 0x69, 0x6e, 0x65, 0x3d, 0x20, 0x69, 0x6e, 0x6e,
        0x6f, 0x64, 0x62, 0xfd, 0x35, 0xbb, 0x4a, 0x02, 0x01, 0x00, 0x00, 0x00, 0x65, 0x00, 0x00,
        0x00, 0x2b, 0x01, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x05, 0x00, 0x00, 0x1a, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x06, 0x03, 0x73, 0x74, 0x64, 0x04, 0x08, 0x00, 0x08, 0x00, 0x08,
        0x00, 0x6d, 0x79, 0x73, 0x71, 0x6c, 0x00, 0x63, 0x72, 0x65, 0x61, 0x74, 0x65, 0x20, 0x74,
        0x61, 0x62, 0x6c, 0x65, 0x20, 0x74, 0x32, 0x28, 0x61, 0x20, 0x69, 0x6e, 0x74, 0x29, 0x20,
        0x65, 0x6e, 0x67, 0x69, 0x6e, 0x65, 0x3d, 0x20, 0x69, 0x6e, 0x6e, 0x6f, 0x64, 0x62, 0xfd,
        0x35, 0xbb, 0x4a, 0x02, 0x01, 0x00, 0x00, 0x00, 0x45, 0x00, 0x00, 0x00, 0x70, 0x01, 0x00,
        0x00, 0x08, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x1a,
        0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x06, 0x03, 0x73, 0x74, 0x64, 0x04, 0x08, 0x00, 0x08, 0x00, 0x08, 0x00, 0x6d, 0x79, 0x73,
        0x71, 0x6c, 0x00, 0x42, 0x45, 0x47, 0x49, 0x4e, 0xfd, 0x35, 0xbb, 0x4a, 0x02, 0x01, 0x00,
        0x00, 0x00, 0x5c, 0x00, 0x00, 0x00, 0xcc, 0x01, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x1a, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00,
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x03, 0x73, 0x74, 0x64, 0x04,
        0x08, 0x00, 0x08, 0x00, 0x08, 0x00, 0x74, 0x65, 0x73, 0x74, 0x00, 0x69, 0x6e, 0x73, 0x65,
        0x72, 0x74, 0x20, 0x69, 0x6e, 0x74, 0x6f, 0x20, 0x74, 0x31, 0x20, 0x28, 0x61, 0x29, 0x20,
        0x76, 0x61, 0x6c, 0x75, 0x65, 0x73, 0x20, 0x28, 0x31, 0x29, 0xfd, 0x35, 0xbb, 0x4a, 0x02,
        0x01, 0x00, 0x00, 0x00, 0x5d, 0x00, 0x00, 0x00, 0x29, 0x02, 0x00, 0x00, 0x00, 0x00, 0x01,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x1a, 0x00, 0x00, 0x00, 0x40,
        0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x03, 0x73, 0x74,
        0x64, 0x04, 0x08, 0x00, 0x08, 0x00, 0x08, 0x00, 0x6d, 0x79, 0x73, 0x71, 0x6c, 0x00, 0x69,
        0x6e, 0x73, 0x65, 0x72, 0x74, 0x20, 0x69, 0x6e, 0x74, 0x6f, 0x20, 0x74, 0x32, 0x20, 0x28,
        0x61, 0x29, 0x20, 0x76, 0x61, 0x6c, 0x75, 0x65, 0x73, 0x20, 0x28, 0x31, 0x29, 0xfd, 0x35,
        0xbb, 0x4a, 0x10, 0x01, 0x00, 0x00, 0x00, 0x1b, 0x00, 0x00, 0x00, 0x44, 0x02, 0x00, 0x00,
        0x00, 0x00, 0x0b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xfd, 0x35, 0xbb, 0x4a, 0x02,
        0x01, 0x00, 0x00, 0x00, 0x64, 0x00, 0x00, 0x00, 0xa8, 0x02, 0x00, 0x00, 0x00, 0x00, 0x01,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x1a, 0x00, 0x00, 0x00, 0x40,
        0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x03, 0x73, 0x74,
        0x64, 0x04, 0x08, 0x00, 0x08, 0x00, 0x08, 0x00, 0x74, 0x65, 0x73, 0x74, 0x00, 0x63, 0x72,
        0x65, 0x61, 0x74, 0x65, 0x20, 0x74, 0x61, 0x62, 0x6c, 0x65, 0x20, 0x74, 0x33, 0x28, 0x61,
        0x20, 0x69, 0x6e, 0x74, 0x29, 0x20, 0x65, 0x6e, 0x67, 0x69, 0x6e, 0x65, 0x3d, 0x20, 0x69,
        0x6e, 0x6e, 0x6f, 0x64, 0x62, 0xfd, 0x35, 0xbb, 0x4a, 0x02, 0x01, 0x00, 0x00, 0x00, 0x65,
        0x00, 0x00, 0x00, 0x0d, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x05, 0x00, 0x00, 0x1a, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x01, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x03, 0x73, 0x74, 0x64, 0x04, 0x08, 0x00, 0x08,
        0x00, 0x08, 0x00, 0x6d, 0x79, 0x73, 0x71, 0x6c, 0x00, 0x63, 0x72, 0x65, 0x61, 0x74, 0x65,
        0x20, 0x74, 0x61, 0x62, 0x6c, 0x65, 0x20, 0x74, 0x34, 0x28, 0x61, 0x20, 0x69, 0x6e, 0x74,
        0x29, 0x20, 0x65, 0x6e, 0x67, 0x69, 0x6e, 0x65, 0x3d, 0x20, 0x6d, 0x79, 0x69, 0x73, 0x61,
        0x6d, 0xfd, 0x35, 0xbb, 0x4a, 0x02, 0x01, 0x00, 0x00, 0x00, 0x45, 0x00, 0x00, 0x00, 0x52,
        0x03, 0x00, 0x00, 0x08, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x00,
        0x00, 0x1a, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x06, 0x03, 0x73, 0x74, 0x64, 0x04, 0x08, 0x00, 0x08, 0x00, 0x08, 0x00, 0x6d,
        0x79, 0x73, 0x71, 0x6c, 0x00, 0x42, 0x45, 0x47, 0x49, 0x4e, 0xfd, 0x35, 0xbb, 0x4a, 0x02,
        0x01, 0x00, 0x00, 0x00, 0x5c, 0x00, 0x00, 0x00, 0xae, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x1a, 0x00, 0x00, 0x00, 0x40,
        0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x03, 0x73, 0x74,
        0x64, 0x04, 0x08, 0x00, 0x08, 0x00, 0x08, 0x00, 0x74, 0x65, 0x73, 0x74, 0x00, 0x69, 0x6e,
        0x73, 0x65, 0x72, 0x74, 0x20, 0x69, 0x6e, 0x74, 0x6f, 0x20, 0x74, 0x33, 0x20, 0x28, 0x61,
        0x29, 0x20, 0x76, 0x61, 0x6c, 0x75, 0x65, 0x73, 0x20, 0x28, 0x32, 0x29, 0xfd, 0x35, 0xbb,
        0x4a, 0x02, 0x01, 0x00, 0x00, 0x00, 0x5d, 0x00, 0x00, 0x00, 0x0b, 0x04, 0x00, 0x00, 0x00,
        0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x1a, 0x00, 0x00,
        0x00, 0x40, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x03,
        0x73, 0x74, 0x64, 0x04, 0x08, 0x00, 0x08, 0x00, 0x08, 0x00, 0x6d, 0x79, 0x73, 0x71, 0x6c,
        0x00, 0x69, 0x6e, 0x73, 0x65, 0x72, 0x74, 0x20, 0x69, 0x6e, 0x74, 0x6f, 0x20, 0x74, 0x34,
        0x20, 0x28, 0x61, 0x29, 0x20, 0x76, 0x61, 0x6c, 0x75, 0x65, 0x73, 0x20, 0x28, 0x32, 0x29,
        0xfd, 0x35, 0xbb, 0x4a, 0x02, 0x01, 0x00, 0x00, 0x00, 0x48, 0x00, 0x00, 0x00, 0x53, 0x04,
        0x00, 0x00, 0x08, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00,
        0x1a, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x06, 0x03, 0x73, 0x74, 0x64, 0x04, 0x08, 0x00, 0x08, 0x00, 0x08, 0x00, 0x6d, 0x79,
        0x73, 0x71, 0x6c, 0x00, 0x52, 0x4f, 0x4c, 0x4c, 0x42, 0x41, 0x43, 0x4b, 0xfd, 0x35, 0xbb,
        0x4a, 0x02, 0x01, 0x00, 0x00, 0x00, 0x61, 0x00, 0x00, 0x00, 0xb4, 0x04, 0x00, 0x00, 0x00,
        0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x1a, 0x00, 0x00,
        0x00, 0x40, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x03,
        0x73, 0x74, 0x64, 0x04, 0x08, 0x00, 0x08, 0x00, 0x08, 0x00, 0x74, 0x65, 0x73, 0x74, 0x00,
        0x63, 0x72, 0x65, 0x61, 0x74, 0x65, 0x20, 0x74, 0x61, 0x62, 0x6c, 0x65, 0x20, 0x74, 0x35,
        0x28, 0x61, 0x20, 0x69, 0x6e, 0x74, 0x29, 0x20, 0x65, 0x6e, 0x67, 0x69, 0x6e, 0x65, 0x3d,
        0x20, 0x4e, 0x44, 0x42, 0xfd, 0x35, 0xbb, 0x4a, 0x02, 0x01, 0x00, 0x00, 0x00, 0x62, 0x00,
        0x00, 0x00, 0x16, 0x05, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x05, 0x00, 0x00, 0x1a, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x03, 0x73, 0x74, 0x64, 0x04, 0x08, 0x00, 0x08, 0x00,
        0x08, 0x00, 0x6d, 0x79, 0x73, 0x71, 0x6c, 0x00, 0x63, 0x72, 0x65, 0x61, 0x74, 0x65, 0x20,
        0x74, 0x61, 0x62, 0x6c, 0x65, 0x20, 0x74, 0x36, 0x28, 0x61, 0x20, 0x69, 0x6e, 0x74, 0x29,
        0x20, 0x65, 0x6e, 0x67, 0x69, 0x6e, 0x65, 0x3d, 0x20, 0x4e, 0x44, 0x42, 0xfd, 0x35, 0xbb,
        0x4a, 0x02, 0x01, 0x00, 0x00, 0x00, 0x45, 0x00, 0x00, 0x00, 0x5b, 0x05, 0x00, 0x00, 0x08,
        0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x1a, 0x00, 0x00,
        0x00, 0x40, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x03,
        0x73, 0x74, 0x64, 0x04, 0x08, 0x00, 0x08, 0x00, 0x08, 0x00, 0x6d, 0x79, 0x73, 0x71, 0x6c,
        0x00, 0x42, 0x45, 0x47, 0x49, 0x4e, 0xfd, 0x35, 0xbb, 0x4a, 0x02, 0x01, 0x00, 0x00, 0x00,
        0x5c, 0x00, 0x00, 0x00, 0xb7, 0x05, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x1a, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x01, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x03, 0x73, 0x74, 0x64, 0x04, 0x08, 0x00,
        0x08, 0x00, 0x08, 0x00, 0x74, 0x65, 0x73, 0x74, 0x00, 0x69, 0x6e, 0x73, 0x65, 0x72, 0x74,
        0x20, 0x69, 0x6e, 0x74, 0x6f, 0x20, 0x74, 0x35, 0x20, 0x28, 0x61, 0x29, 0x20, 0x76, 0x61,
        0x6c, 0x75, 0x65, 0x73, 0x20, 0x28, 0x33, 0x29, 0xfd, 0x35, 0xbb, 0x4a, 0x02, 0x01, 0x00,
        0x00, 0x00, 0x5d, 0x00, 0x00, 0x00, 0x14, 0x06, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x1a, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00,
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x03, 0x73, 0x74, 0x64, 0x04,
        0x08, 0x00, 0x08, 0x00, 0x08, 0x00, 0x6d, 0x79, 0x73, 0x71, 0x6c, 0x00, 0x69, 0x6e, 0x73,
        0x65, 0x72, 0x74, 0x20, 0x69, 0x6e, 0x74, 0x6f, 0x20, 0x74, 0x36, 0x20, 0x28, 0x61, 0x29,
        0x20, 0x76, 0x61, 0x6c, 0x75, 0x65, 0x73, 0x20, 0x28, 0x33, 0x29, 0xfd, 0x35, 0xbb, 0x4a,
        0x02, 0x01, 0x00, 0x00, 0x00, 0x46, 0x00, 0x00, 0x00, 0x5a, 0x06, 0x00, 0x00, 0x08, 0x00,
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x1a, 0x00, 0x00, 0x00,
        0x40, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x03, 0x73,
        0x74, 0x64, 0x04, 0x08, 0x00, 0x08, 0x00, 0x08, 0x00, 0x6d, 0x79, 0x73, 0x71, 0x6c, 0x00,
        0x43, 0x4f, 0x4d, 0x4d, 0x49, 0x54, 0xfd, 0x35, 0xbb, 0x4a, 0x04, 0x01, 0x00, 0x00, 0x00,
        0x2c, 0x00, 0x00, 0x00, 0x86, 0x06, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x6d, 0x61, 0x73, 0x74, 0x65, 0x72, 0x2d, 0x62, 0x69, 0x6e, 0x2e, 0x30,
        0x30, 0x30, 0x30, 0x30, 0x32,
    ];

    #[test]
    fn binlog_file_header_roundtrip() -> io::Result<()> {
        let mut output = Vec::new();

        let binlog_file_header = BinlogFileHeader::read(
            4,
            &FormatDescriptionEvent::new(BinlogVersion::Version4),
            BINLOG_FILE,
        )?;
        binlog_file_header.write(BinlogVersion::Version4, &mut output)?;

        assert_eq!(&output[..], &BINLOG_FILE[..BinlogFileHeader::LEN]);

        Ok(())
    }

    #[test]
    fn binlog_file_iterator() -> io::Result<()> {
        let binlog_file = BinlogFile::new(BinlogVersion::Version4, BINLOG_FILE)?;

        let mut total = 0;
        let mut ev_pos = 4;

        for (i, ev) in binlog_file.enumerate() {
            let data_start = ev_pos + BinlogEventHeader::LEN;
            let ev = ev?;
            match i {
                0 => {
                    assert_eq!(
                        ev.header,
                        BinlogEventHeader {
                            timestamp: 1253783036,
                            event_type: RawField::new(15),
                            server_id: 1,
                            event_size: 94,
                            log_pos: 98,
                            flags: RawFlags(0),
                        }
                    );
                }
                1 => assert_eq!(
                    ev.header,
                    BinlogEventHeader {
                        timestamp: 1253783037,
                        event_type: RawField::new(2),
                        server_id: 1,
                        event_size: 100,
                        log_pos: 198,
                        flags: RawFlags(0),
                    }
                ),
                2 => assert_eq!(
                    ev.header,
                    BinlogEventHeader {
                        timestamp: 1253783037,
                        event_type: RawField::new(2),
                        server_id: 1,
                        event_size: 101,
                        log_pos: 299,
                        flags: RawFlags(0),
                    }
                ),
                3 => assert_eq!(
                    ev.header,
                    BinlogEventHeader {
                        timestamp: 1253783037,
                        event_type: RawField::new(2),
                        server_id: 1,
                        event_size: 69,
                        log_pos: 368,
                        flags: RawFlags(8),
                    }
                ),
                4 => assert_eq!(
                    ev.header,
                    BinlogEventHeader {
                        timestamp: 1253783037,
                        event_type: RawField::new(2),
                        server_id: 1,
                        event_size: 92,
                        log_pos: 460,
                        flags: RawFlags(0),
                    }
                ),
                5 => assert_eq!(
                    ev.header,
                    BinlogEventHeader {
                        timestamp: 1253783037,
                        event_type: RawField::new(2),
                        server_id: 1,
                        event_size: 93,
                        log_pos: 553,
                        flags: RawFlags(0),
                    }
                ),
                6 => assert_eq!(
                    ev.header,
                    BinlogEventHeader {
                        timestamp: 1253783037,
                        event_type: RawField::new(16),
                        server_id: 1,
                        event_size: 27,
                        log_pos: 580,
                        flags: RawFlags(0),
                    }
                ),
                7 => assert_eq!(
                    ev.header,
                    BinlogEventHeader {
                        timestamp: 1253783037,
                        event_type: RawField::new(2),
                        server_id: 1,
                        event_size: 100,
                        log_pos: 680,
                        flags: RawFlags(0),
                    }
                ),
                8 => assert_eq!(
                    ev.header,
                    BinlogEventHeader {
                        timestamp: 1253783037,
                        event_type: RawField::new(2),
                        server_id: 1,
                        event_size: 101,
                        log_pos: 781,
                        flags: RawFlags(0),
                    }
                ),
                9 => assert_eq!(
                    ev.header,
                    BinlogEventHeader {
                        timestamp: 1253783037,
                        event_type: RawField::new(2),
                        server_id: 1,
                        event_size: 69,
                        log_pos: 850,
                        flags: RawFlags(8),
                    }
                ),
                10 => assert_eq!(
                    ev.header,
                    BinlogEventHeader {
                        timestamp: 1253783037,
                        event_type: RawField::new(2),
                        server_id: 1,
                        event_size: 92,
                        log_pos: 942,
                        flags: RawFlags(0),
                    }
                ),
                11 => assert_eq!(
                    ev.header,
                    BinlogEventHeader {
                        timestamp: 1253783037,
                        event_type: RawField::new(2),
                        server_id: 1,
                        event_size: 93,
                        log_pos: 1035,
                        flags: RawFlags(0),
                    }
                ),
                12 => assert_eq!(
                    ev.header,
                    BinlogEventHeader {
                        timestamp: 1253783037,
                        event_type: RawField::new(2),
                        server_id: 1,
                        event_size: 72,
                        log_pos: 1107,
                        flags: RawFlags(8),
                    }
                ),
                13 => assert_eq!(
                    ev.header,
                    BinlogEventHeader {
                        timestamp: 1253783037,
                        event_type: RawField::new(2),
                        server_id: 1,
                        event_size: 97,
                        log_pos: 1204,
                        flags: RawFlags(0),
                    }
                ),
                14 => assert_eq!(
                    ev.header,
                    BinlogEventHeader {
                        timestamp: 1253783037,
                        event_type: RawField::new(2),
                        server_id: 1,
                        event_size: 98,
                        log_pos: 1302,
                        flags: RawFlags(0),
                    }
                ),
                15 => assert_eq!(
                    ev.header,
                    BinlogEventHeader {
                        timestamp: 1253783037,
                        event_type: RawField::new(2),
                        server_id: 1,
                        event_size: 69,
                        log_pos: 1371,
                        flags: RawFlags(8),
                    }
                ),
                16 => assert_eq!(
                    ev.header,
                    BinlogEventHeader {
                        timestamp: 1253783037,
                        event_type: RawField::new(2),
                        server_id: 1,
                        event_size: 92,
                        log_pos: 1463,
                        flags: RawFlags(0),
                    }
                ),
                17 => assert_eq!(
                    ev.header,
                    BinlogEventHeader {
                        timestamp: 1253783037,
                        event_type: RawField::new(2),
                        server_id: 1,
                        event_size: 93,
                        log_pos: 1556,
                        flags: RawFlags(0),
                    }
                ),
                18 => assert_eq!(
                    ev.header,
                    BinlogEventHeader {
                        timestamp: 1253783037,
                        event_type: RawField::new(2),
                        server_id: 1,
                        event_size: 70,
                        log_pos: 1626,
                        flags: RawFlags(8),
                    }
                ),
                19 => assert_eq!(
                    ev.header,
                    BinlogEventHeader {
                        timestamp: 1253783037,
                        event_type: RawField::new(4),
                        server_id: 1,
                        event_size: 44,
                        log_pos: 1670,
                        flags: RawFlags(0),
                    }
                ),
                _ => panic!("too many"),
            }

            assert_eq!(
                ev.data,
                &BINLOG_FILE[data_start
                    ..(data_start + ev.header.event_size as usize - BinlogEventHeader::LEN)],
            );

            total += 1;
            ev_pos = ev.header.log_pos as usize;
        }

        assert_eq!(total, 20);
        Ok(())
    }

    #[test]
    fn binlog_event_roundtrip() -> io::Result<()> {
        let files = [
            "./test-data/binlogs/bug32407.001",
            "./test-data/binlogs/bug11747887-bin.000003",
            "./test-data/binlogs/update-full-row.binlog",
            "./test-data/binlogs/update-partial-row.binlog",
            "./test-data/binlogs/ver_5_1_23.001",
            "./test-data/binlogs/ver_5_1-wl2325_r.001",
            "./test-data/binlogs/ver_5_1-wl2325_s.001",
            "./test-data/binlogs/ver_trunk_row_v2.001",
            "./test-data/binlogs/write-full-row.binlog",
            "./test-data/binlogs/write-partial-row.binlog",
        ];

        'outer: for file_name in &files {
            let file_data = std::fs::read(file_name)?;
            let mut binlog_file = BinlogFile::new(BinlogVersion::Version4, &file_data[..])?;

            let mut ev_pos = 4;

            while let Some(ev) = binlog_file.next() {
                let ev = ev?;
                let ev_end = ev_pos + ev.header.event_size as usize;
                let binlog_version = binlog_file
                    .reader
                    .fde
                    .binlog_version
                    .get()
                    .unwrap_or(BinlogVersion::Version4);

                let mut output = Vec::new();
                ev.write(binlog_version, &mut output)?;
                assert_eq!(output, &file_data[ev_pos..ev_end]);

                let event = match ev.read_data() {
                    Ok(event) => event.unwrap(),
                    Err(err)
                        if err.kind() == std::io::ErrorKind::Other
                            && ev.header.event_type.get() == Ok(EventType::XID_EVENT)
                            && ev.header.event_size == 0x26
                            && *file_name == "./test-data/binlogs/ver_5_1-wl2325_r.001" =>
                    {
                        // Testfile contains broken xid event.
                        continue 'outer;
                    }
                    other => other.transpose().unwrap()?,
                };

                output = Vec::new();
                event.write(binlog_version, &mut output)?;

                if matches!(event, EventData::UserVarEvent(_)) {
                    // Server may or may not write the flags field, but we will always write it.
                    assert_eq!(&output[..ev.data.len()], &ev.data[..]);
                    assert!(output.len() == ev.data.len() || output.len() == ev.data.len() + 1);
                } else {
                    assert_eq!(output, ev.data);
                }

                ev_pos = ev_end;
            }
        }

        Ok(())
    }
}
