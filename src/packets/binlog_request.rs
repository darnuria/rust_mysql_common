// Copyright (c) 2021 Anatoly Ikorsky
//
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or http://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. All files in the project carrying such notice may not be copied,
// modified, or distributed except according to those terms.

use std::borrow::Cow;

use crate::misc::raw::Either;

use super::{BinlogDumpFlags, ComBinlogDump, ComBinlogDumpGtid, Sid};

/// Binlog request representation. Please consult MySql documentation.
///
/// This struct is a helper builder for [`ComBinlogDump`] and [`ComBinlogDumpGtid`].
///
/// `server_id`, `host`, `port` are inspectable Source server side with:
/// `SHOW SLAVE HOSTS` mysql 5.7 or `SHOW REPLICAS` on mysql 8.x.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct BinlogRequest<'a> {
    /// Server id of a slave.
    server_id: u32,
    /// If true, then `COM_BINLOG_DUMP_GTID` will be used.
    use_gtid: bool,
    /// If `use_gtid` is `false`, then all flags except `BINLOG_DUMP_NON_BLOCK` will be truncated.
    flags: BinlogDumpFlags,
    /// Filename of the binlog on the master.
    filename: Cow<'a, [u8]>,
    /// Replicat/Slave hostname
    ///
    /// Defaults to the Empty string.
    hostname: Cow<'a, [u8]>,
    /// Replicat/Slave port
    ///
    /// Defaults to 0.
    port: u16,
    /// Position in the binlog-file to start the stream with.
    ///
    /// If `use_gtid` is `false`, then the value will be truncated to u32.
    pos: u64,
    /// SID blocks. If `use_gtid` is `false`, then this value is ignored.
    sids: Vec<Sid<'a>>,
}

impl<'a> BinlogRequest<'a> {
    /// Creates new request with the given slave server id.
    pub fn new(server_id: u32) -> Self {
        Self {
            server_id,
            use_gtid: false,
            flags: BinlogDumpFlags::empty(),
            filename: Default::default(),
            pos: 4,
            sids: vec![],
            hostname: Default::default(),
            port: 0,
        }
    }

    /// Server id of a slave.
    pub fn server_id(&self) -> u32 {
        self.server_id
    }

    /// If true, then `COM_BINLOG_DUMP_GTID` will be used (defaults to `false`).
    pub fn use_gtid(&self) -> bool {
        self.use_gtid
    }

    /// Returns the hostname to report to the Source server used for replication.
    ///
    /// Purely informative it's not an information used for connection.
    /// Be sure to set something meaningful.
    pub fn hostname_raw(&'a self) -> &'a [u8] {
        self.hostname.as_ref()
    }

    /// Returns the hostname to report to the Source server used for replication,
    /// as a UTF-8 string (lossy converted).
    ///
    /// Purely informative it's not an information used for connection.
    /// Be sure to set something meaningful.
    pub fn hostname(&'a self) -> Cow<'a, str> {
        String::from_utf8_lossy(self.hostname.as_ref())
    }

    /// If `use_gtid` is `false`, then all flags except `BINLOG_DUMP_NON_BLOCK` will be truncated
    /// (defaults to empty).
    pub fn flags(&self) -> BinlogDumpFlags {
        self.flags
    }

    /// Filename of the binlog on the master (defaults to an empty string).
    pub fn filename_raw(&'a self) -> &'a [u8] {
        &self.filename.as_ref()
    }

    /// Filename of the binlog on the master as a UTF-8 string (lossy converted)
    /// (defaults to an empty string).
    pub fn filename(&'a self) -> &'a [u8] {
        &self.filename.as_ref()
    }

    /// Position in the binlog-file to start the stream with (defaults to `4`).
    ///
    /// If `use_gtid` is `false`, then the value will be truncated to u32.
    pub fn pos(&self) -> u64 {
        self.pos
    }

    /// Port to report to the Source used for Replication.
    ///
    /// Purely informative be sure to define the same as the one used for connection.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// If `use_gtid` is `false`, then this value will be ignored (defaults to an empty vector).
    pub fn sids(&self) -> &[Sid<'_>] {
        &self.sids
    }

    /// Returns modified `self` with the given value of the `server_id` field.
    pub fn with_server_id(mut self, server_id: u32) -> Self {
        self.server_id = server_id;
        self
    }

    /// Returns modified `self` with the given `host` value,
    ///
    /// The host value is purely informative and used in mysql replica inspection statements.
    pub fn with_hostname(mut self, hostname: impl Into<Cow<'a, [u8]>>) -> Self {
        self.hostname = hostname.into();
        self
    }

    /// Returns modified `self` with the given `port` value to show in
    ///
    /// ## Warning
    ///
    /// Setting a reporting port different of the real port used to stream
    /// the binlog will lead replica inspections sql statement to show the port setted here!
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Returns modified `self` with the given value of the `use_gtid` field.
    pub fn with_use_gtid(mut self, use_gtid: bool) -> Self {
        self.use_gtid = use_gtid;
        self
    }

    /// Returns modified `self` with the given value of the `flags` field.
    pub fn with_flags(mut self, flags: BinlogDumpFlags) -> Self {
        self.flags = flags;
        self
    }

    /// Returns modified `self` with the given value of the `filename` field.
    pub fn with_filename(mut self, filename: impl Into<Cow<'a, [u8]>>) -> Self {
        self.filename = filename.into();
        self
    }

    /// Returns modified `self` with the given value of the `pos` field.
    pub fn with_pos<T: Into<u64>>(mut self, pos: T) -> Self {
        self.pos = pos.into();
        self
    }

    /// Returns modified `self` with the given value of the `sid_blocks` field.
    pub fn with_sids<T>(mut self, sids: T) -> Self
    where
        T: IntoIterator<Item = Sid<'a>>,
    {
        self.sids = sids.into_iter().collect();
        self
    }

    pub fn as_cmd(&self) -> Either<ComBinlogDump<'_>, ComBinlogDumpGtid<'_>> {
        if self.use_gtid() {
            let cmd = ComBinlogDumpGtid::new(self.server_id)
                .with_pos(self.pos)
                .with_flags(self.flags)
                .with_filename(&*self.filename)
                .with_sids(&*self.sids);
            Either::Right(cmd)
        } else {
            let cmd = ComBinlogDump::new(self.server_id)
                .with_pos(self.pos as u32)
                .with_filename(&*self.filename)
                .with_flags(self.flags & BinlogDumpFlags::BINLOG_DUMP_NON_BLOCK);
            Either::Left(cmd)
        }
    }
}
