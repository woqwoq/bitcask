use std::{
    array::TryFromSliceError, io::Error, num::ParseIntError, path::PathBuf, time::SystemTimeError,
};

#[derive(Debug)]
pub enum BitcaskError {
    DatafileError(DatafileError),

    DatafileMissing(usize),
    InvalidDBPath(Error),
    FilenameConversion,
    DatafileNameError(ParseIntError),
    PathIsFile,
    RecordLargerThanMaxDatafileSize(usize),
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

#[derive(Debug)]
pub enum DatafileError {
    NegativeTimestamp(SystemTimeError),
    SliceConversion(TryFromSliceError),
    DatafileFull,
    OversizedDatafileRecord,
    UndersizedDatafileHeader(usize),
    UndersizedDatafileKey { actual: usize, expected: usize },
    UndersizedDatafileValue { actual: usize, expected: usize },
    CrcMismatch,
    IOWrite(Error),
    InvalidPath(PathBuf),
    WriterFileOffsetOverflow,
    WriteAttemptToReaderDatafile,
    UnexpectedDatafileRecordType,
    SyncError(Error),
}

impl From<Error> for DatafileError {
    fn from(value: Error) -> Self {
        Self::IOWrite(value)
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
