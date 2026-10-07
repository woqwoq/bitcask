use std::{array::TryFromSliceError, fmt, io::Error, num::ParseIntError, time::SystemTimeError};

use crate::datafile::DATAFILE_HEADER_SIZE_BYTES;

#[derive(Debug)]
pub enum BitcaskError {
    DatafileError(DatafileError),

    DatafileMissing(usize),
    InvalidDBPath(Error),
    FilenameConversion,
    DatafileNameError(ParseIntError),
    PathIsFile,
    RecordLargerThanMaxDatafileSize(usize),
    FatalReplayFailure(DatafileError),
}

impl From<ParseIntError> for BitcaskError {
    fn from(value: ParseIntError) -> Self {
        Self::DatafileNameError(value)
    }
}

impl From<Error> for BitcaskError {
    fn from(value: Error) -> Self {
        Self::InvalidDBPath(value)
    }
}

impl From<DatafileError> for BitcaskError {
    fn from(value: DatafileError) -> Self {
        Self::DatafileError(value)
    }
}

impl fmt::Display for BitcaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BitcaskError::DatafileError(e) => write!(f, "datafile error: {e}"),
            BitcaskError::DatafileMissing(id) => write!(
                f,
                "datafile with id {id} is missing: the file could not be found on disk"
            ),
            BitcaskError::InvalidDBPath(e) => {
                write!(f, "failed to access or create the database directory: {e}")
            }
            BitcaskError::FilenameConversion => {
                write!(f, "encountered a datafile filename that is not valid UTF-8")
            }
            BitcaskError::DatafileNameError(e) => write!(
                f,
                "datafile filename does not contain a valid numeric id: {e}"
            ),
            BitcaskError::PathIsFile => write!(
                f,
                "the database path points to a file, but a directory is required"
            ),
            BitcaskError::RecordLargerThanMaxDatafileSize(size) => write!(
                f,
                "record of {size} bytes is larger than the maximum datafile size and cannot be written"
            ),
            BitcaskError::FatalReplayFailure(e) => write!(
                f,
                "fatal error while replaying datafiles during startup: {e}"
            ),
        }
    }
}

#[derive(Debug)]
pub enum DatafileError {
    NegativeTimestamp(SystemTimeError),
    SliceConversion(TryFromSliceError),
    OversizedDatafileRecord,
    UndersizedDatafileHeader(usize),
    UndersizedDatafileKey { actual: usize, expected: usize },
    UndersizedDatafileValue { actual: usize, expected: usize },
    CrcMismatch,
    IO(Error),
    WriterFileOffsetOverflow,
    WriteAttemptToReaderDatafile,
    UnexpectedDatafileRecordType,
    SyncError(Error),
    TornWrite,
    TruncateFailure(Error),
}

impl From<Error> for DatafileError {
    fn from(value: Error) -> Self {
        Self::IO(value)
    }
}

impl From<SystemTimeError> for DatafileError {
    fn from(value: SystemTimeError) -> Self {
        Self::NegativeTimestamp(value)
    }
}

impl From<TryFromSliceError> for DatafileError {
    fn from(value: TryFromSliceError) -> Self {
        Self::SliceConversion(value)
    }
}

impl fmt::Display for DatafileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DatafileError::NegativeTimestamp(e) => write!(
                f,
                "system clock is before the UNIX epoch, cannot derive a record timestamp: {e}"
            ),
            DatafileError::SliceConversion(e) => write!(
                f,
                "failed to convert a byte slice into a fixed-size array while decoding: {e}"
            ),
            DatafileError::OversizedDatafileRecord => write!(
                f,
                "record is too large to be represented in the datafile header"
            ),
            DatafileError::UndersizedDatafileHeader(actual) => write!(
                f,
                "incomplete datafile header: got {actual} bytes, expected at least {DATAFILE_HEADER_SIZE_BYTES}"
            ),
            DatafileError::UndersizedDatafileKey { actual, expected } => write!(
                f,
                "datafile record ends before the key is complete: expected at least {expected} bytes, got {actual}"
            ),
            DatafileError::UndersizedDatafileValue { actual, expected } => write!(
                f,
                "datafile record ends before the value is complete: expected at least {expected} bytes, got {actual}"
            ),
            DatafileError::CrcMismatch => write!(f, "CRC mismatch: the record is corrupted"),
            DatafileError::IO(e) => write!(f, "I/O error: {e}"),
            DatafileError::WriterFileOffsetOverflow => write!(
                f,
                "writer offset overflow: advancing the write offset exceeds the maximum file size"
            ),
            DatafileError::WriteAttemptToReaderDatafile => write!(
                f,
                "attempted to append a record to a datafile opened in read-only mode"
            ),
            DatafileError::UnexpectedDatafileRecordType => write!(
                f,
                "unexpected record type: expected 0 (normal) or 1 (tombstone)"
            ),
            DatafileError::SyncError(e) => write!(f, "failed to sync the datafile to disk: {e}"),
            DatafileError::TornWrite => write!(
                f,
                "torn write detected: the datafile ends in the middle of a record"
            ),
            DatafileError::TruncateFailure(e) => {
                write!(f, "failed to truncate the datafile: {e}")
            }
        }
    }
}
