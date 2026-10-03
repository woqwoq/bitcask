use std::{array::TryFromSliceError, io::Error, path::PathBuf, time::SystemTimeError};

#[derive(Debug)]
pub enum BitcaskError {
    DatafileError(DatafileError),

    DatafileMissing(usize),
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
    OversizedDatafileEntry,
    UndersizedDatafileHeader(usize),
    UndersizedDatafileKey { actual: usize, expected: usize },
    UndersizedDatafileValue { actual: usize, expected: usize },
    CrcMismatch,
    IOWrite(Error),
    InvalidPath(PathBuf),
    WriterFileOffsetOverflow,
    WriteAttemptToReaderDatafile,
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
