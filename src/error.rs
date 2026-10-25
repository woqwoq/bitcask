use std::{array::TryFromSliceError, fmt, io, num::ParseIntError, time::SystemTimeError};

use crate::options::DATAFILE_HEADER_SIZE_BYTES;

#[derive(Debug)]
pub enum BitcaskError {
    FatalOpenFailure(OpenError),
    GetFailure(GetError),
    PutFailure(PutError),
    RemoveFailure(RemoveError),
    FatalMergeFailure(MergeError),
    SyncFailure(DatafileError),
}

impl fmt::Display for BitcaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BitcaskError::FatalOpenFailure(e) => write!(f, "bitcask open failure: {e}"),
            BitcaskError::GetFailure(e) => write!(f, "bitcask get failure: {e}"),
            BitcaskError::PutFailure(e) => write!(f, "bitcask put failure: {e}"),
            BitcaskError::RemoveFailure(e) => write!(f, "bitcask remove failure: {e}"),
            BitcaskError::FatalMergeFailure(e) => write!(f, "bitcask merge failure: {e}"),
            BitcaskError::SyncFailure(e) => write!(f, "bitcask file sync failure: {e}"),
        }
    }
}

impl std::error::Error for BitcaskError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            BitcaskError::FatalOpenFailure(e) => Some(e),
            BitcaskError::GetFailure(e) => Some(e),
            BitcaskError::PutFailure(e) => Some(e),
            BitcaskError::RemoveFailure(e) => Some(e),
            BitcaskError::FatalMergeFailure(e) => Some(e),
            BitcaskError::SyncFailure(e) => Some(e),
        }
    }
}

impl From<OpenError> for BitcaskError {
    fn from(value: OpenError) -> Self {
        Self::FatalOpenFailure(value)
    }
}

impl From<GetError> for BitcaskError {
    fn from(value: GetError) -> Self {
        Self::GetFailure(value)
    }
}

impl From<PutError> for BitcaskError {
    fn from(value: PutError) -> Self {
        Self::PutFailure(value)
    }
}

impl From<RemoveError> for BitcaskError {
    fn from(value: RemoveError) -> Self {
        Self::RemoveFailure(value)
    }
}

impl From<MergeError> for BitcaskError {
    fn from(value: MergeError) -> Self {
        Self::FatalMergeFailure(value)
    }
}

#[derive(Debug)]
pub enum OpenError {
    InvalidDBPath(io::Error),
    PathIsFile,
    DatafileNameError(ParseIntError),
    FilenameConversion,
    FatalDatafileReplay(DatafileError),
    FatalHintfileReplay(HintfileError),
    WriterInit(DatafileError),
    InvalidDBOptions(InvalidOptionError),
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpenError::InvalidDBPath(e) => {
                write!(f, "failed to access or create the database directory: {e}")
            }
            OpenError::PathIsFile => write!(
                f,
                "the database path points to a file, but a directory is required"
            ),
            OpenError::DatafileNameError(e) => write!(
                f,
                "datafile filename does not contain a valid numeric id: {e}"
            ),
            OpenError::FilenameConversion => {
                write!(f, "encountered a datafile filename that is not valid UTF-8")
            }
            OpenError::FatalDatafileReplay(e) => {
                write!(
                    f,
                    "fatal error while replaying datafiles during startup: {e}"
                )
            }
            OpenError::FatalHintfileReplay(e) => {
                write!(
                    f,
                    "fatal error while replaying hintfiles during startup: {e}"
                )
            }
            OpenError::WriterInit(e) => {
                write!(f, "failed to initialize active writer datafile: {e}")
            }
            OpenError::InvalidDBOptions(e) => {
                write!(f, "invalid DB options detected: {e}")
            }
        }
    }
}

impl std::error::Error for OpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            OpenError::InvalidDBPath(e) => Some(e),
            OpenError::DatafileNameError(e) => Some(e),
            OpenError::FatalDatafileReplay(e) | OpenError::WriterInit(e) => Some(e),
            OpenError::FatalHintfileReplay(e) => Some(e),
            _ => None,
        }
    }
}

impl From<DatafileError> for OpenError {
    fn from(value: DatafileError) -> Self {
        Self::FatalDatafileReplay(value)
    }
}

impl From<HintfileError> for OpenError {
    fn from(value: HintfileError) -> Self {
        Self::FatalHintfileReplay(value)
    }
}

impl From<ParseIntError> for OpenError {
    fn from(value: ParseIntError) -> Self {
        Self::DatafileNameError(value)
    }
}

#[derive(Debug)]
pub enum GetError {
    Datafile(DatafileError),
    DatafileMissing(u32),
}

impl fmt::Display for GetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GetError::Datafile(e) => write!(f, "datafile error during get operation: {e}"),
            GetError::DatafileMissing(id) => write!(
                f,
                "datafile with id {id} is missing: the file could not be found on disk"
            ),
        }
    }
}

impl std::error::Error for GetError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            GetError::Datafile(e) => Some(e),
            GetError::DatafileMissing(_) => None,
        }
    }
}

impl From<DatafileError> for GetError {
    fn from(value: DatafileError) -> Self {
        Self::Datafile(value)
    }
}

#[derive(Debug)]
pub enum PutError {
    RecordLargerThanMaxDatafileSize(usize),
    Datafile(DatafileError),
    AutoMerge(MergeError),
}

impl fmt::Display for PutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PutError::RecordLargerThanMaxDatafileSize(size) => write!(
                f,
                "record of {size} bytes is larger than the maximum datafile size and cannot be written"
            ),
            PutError::Datafile(e) => write!(f, "datafile error during put operation: {e}"),
            PutError::AutoMerge(e) => write!(f, "auto-merge failed during put operation: {e}"),
        }
    }
}

impl std::error::Error for PutError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PutError::Datafile(e) => Some(e),
            PutError::AutoMerge(e) => Some(e),
            _ => None,
        }
    }
}

impl From<DatafileError> for PutError {
    fn from(value: DatafileError) -> Self {
        Self::Datafile(value)
    }
}

impl From<MergeError> for PutError {
    fn from(value: MergeError) -> Self {
        Self::AutoMerge(value)
    }
}

#[derive(Debug)]
pub enum RemoveError {
    Datafile(DatafileError),
    AutoMerge(MergeError),
}

impl fmt::Display for RemoveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RemoveError::Datafile(e) => write!(f, "datafile error during remove operation: {e}"),
            RemoveError::AutoMerge(e) => {
                write!(f, "auto-merge failed during remove operation: {e}")
            }
        }
    }
}

impl std::error::Error for RemoveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RemoveError::Datafile(e) => Some(e),
            RemoveError::AutoMerge(e) => Some(e),
        }
    }
}

impl From<DatafileError> for RemoveError {
    fn from(value: DatafileError) -> Self {
        Self::Datafile(value)
    }
}

impl From<MergeError> for RemoveError {
    fn from(value: MergeError) -> Self {
        Self::AutoMerge(value)
    }
}

#[derive(Debug)]
pub enum MergeError {
    Datafile(DatafileError),
    Hintfile(HintfileError),
    FileCleanup(io::Error),
    DirectoryScan(io::Error),
}

impl fmt::Display for MergeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MergeError::Datafile(e) => write!(f, "datafile error during merge: {e}"),
            MergeError::Hintfile(e) => write!(f, "hintfile error during merge: {e}"),
            MergeError::FileCleanup(e) => {
                write!(f, "failed to clean up old file after merge: {e}")
            }
            MergeError::DirectoryScan(e) => {
                write!(
                    f,
                    "failed to scan directory for datafiles during merge: {e}"
                )
            }
        }
    }
}

impl std::error::Error for MergeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MergeError::Datafile(e) => Some(e),
            MergeError::Hintfile(e) => Some(e),
            MergeError::FileCleanup(e) | MergeError::DirectoryScan(e) => Some(e),
        }
    }
}

impl From<DatafileError> for MergeError {
    fn from(value: DatafileError) -> Self {
        Self::Datafile(value)
    }
}

impl From<HintfileError> for MergeError {
    fn from(value: HintfileError) -> Self {
        Self::Hintfile(value)
    }
}

#[derive(Debug)]
pub enum DatafileError {
    // IO operations
    Open(io::Error),
    Read { offset: u64, source: io::Error },
    Write(io::Error),
    Flush(io::Error),
    Sync(io::Error),
    Truncate(io::Error),
    DatafileNotFound(io::Error),

    // Domain/validation variants
    NegativeTimestamp(SystemTimeError),
    SliceConversion(TryFromSliceError),
    OversizedDatafileRecord,
    UndersizedDatafileHeader(usize),
    UndersizedDatafileKey { actual: usize, expected: usize },
    UndersizedDatafileValue { actual: usize, expected: usize },
    CrcMismatch,
    WriterFileOffsetOverflow,
    WriteAttemptToReaderDatafile,
    UnexpectedDatafileRecordType,
    TornRecord,
}

impl fmt::Display for DatafileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DatafileError::Open(e) => write!(f, "failed to open datafile: {e}"),
            DatafileError::Read { offset, source } => {
                write!(f, "failed to read datafile at offset {offset}: {source}")
            }
            DatafileError::Write(e) => write!(f, "failed to write to datafile: {e}"),
            DatafileError::Flush(e) => write!(f, "failed to flush datafile: {e}"),
            DatafileError::Sync(e) => write!(f, "failed to sync datafile to disk: {e}"),
            DatafileError::Truncate(e) => write!(f, "failed to truncate datafile: {e}"),
            DatafileError::DatafileNotFound(e) => write!(f, "datafile not found: {e}"),
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
            DatafileError::TornRecord => write!(
                f,
                "torn write detected: the datafile ends in the middle of a record"
            ),
        }
    }
}

impl std::error::Error for DatafileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DatafileError::Open(e)
            | DatafileError::Write(e)
            | DatafileError::Flush(e)
            | DatafileError::Sync(e)
            | DatafileError::Truncate(e)
            | DatafileError::DatafileNotFound(e) => Some(e),
            DatafileError::Read { source, .. } => Some(source),
            DatafileError::NegativeTimestamp(e) => Some(e),
            DatafileError::SliceConversion(e) => Some(e),
            _ => None,
        }
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

#[derive(Debug)]
pub enum HintfileError {
    // IO operations
    Open(io::Error),
    Read(io::Error),
    Write(io::Error),
    Flush(io::Error),
    Sync(io::Error),

    // Validation variants
    SliceConversion(TryFromSliceError),
    OversizedHintfileRecord,
    UndersizedHintfileHeader(usize),
    UndersizedHintfileKey { actual: usize, expected: usize },
    CrcMismatch,
}

impl fmt::Display for HintfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HintfileError::Open(e) => write!(f, "failed to open hintfile: {e}"),
            HintfileError::Read(e) => write!(f, "failed to read hintfile: {e}"),
            HintfileError::Write(e) => write!(f, "failed to write to hintfile: {e}"),
            HintfileError::Flush(e) => write!(f, "failed to flush hintfile: {e}"),
            HintfileError::Sync(e) => write!(f, "failed to sync hintfile: {e}"),
            HintfileError::SliceConversion(e) => write!(
                f,
                "failed to convert a byte slice into a fixed-size array while decoding hintfile: {e}"
            ),
            HintfileError::OversizedHintfileRecord => {
                write!(f, "hintfile record is too large")
            }
            HintfileError::UndersizedHintfileHeader(actual) => {
                write!(f, "incomplete hintfile header: got {actual} bytes")
            }
            HintfileError::UndersizedHintfileKey { actual, expected } => write!(
                f,
                "hintfile record ends before the key is complete: expected at least {expected} bytes, got {actual}"
            ),
            HintfileError::CrcMismatch => {
                write!(f, "CRC mismatch: hintfile record is corrupted")
            }
        }
    }
}

impl std::error::Error for HintfileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            HintfileError::Open(e)
            | HintfileError::Read(e)
            | HintfileError::Write(e)
            | HintfileError::Flush(e)
            | HintfileError::Sync(e) => Some(e),
            HintfileError::SliceConversion(e) => Some(e),
            _ => None,
        }
    }
}

impl From<TryFromSliceError> for HintfileError {
    fn from(value: TryFromSliceError) -> Self {
        Self::SliceConversion(value)
    }
}

#[derive(Debug)]
pub enum InvalidOptionError {
    NonZeroField(String),
    NonEmptyField(String),
}

impl fmt::Display for InvalidOptionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InvalidOptionError::NonZeroField(field) => {
                write!(f, "field {field} can't be set to zero.")
            }
            InvalidOptionError::NonEmptyField(field) => {
                write!(f, "field {field} can't be empty.")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as StdError;

    #[test]
    fn datafile_error_display_and_source() {
        let io_err = io::Error::other("disk failed");
        let df_err = DatafileError::Open(io_err);
        assert!(format!("{df_err}").contains("failed to open datafile"));
        assert!(df_err.source().is_some());

        let read_err = DatafileError::Read {
            offset: 42,
            source: io::Error::new(io::ErrorKind::UnexpectedEof, "eof"),
        };
        assert!(format!("{read_err}").contains("offset 42"));
        assert!(read_err.source().is_some());

        let torn = DatafileError::TornRecord;
        assert!(format!("{torn}").contains("torn write"));
        assert!(torn.source().is_none());
    }

    #[test]
    fn hintfile_error_display_and_source() {
        let io_err = io::Error::new(io::ErrorKind::PermissionDenied, "denied");
        let hf_err = HintfileError::Write(io_err);
        assert!(format!("{hf_err}").contains("failed to write to hintfile"));
        assert!(hf_err.source().is_some());

        let crc = HintfileError::CrcMismatch;
        assert!(format!("{crc}").contains("CRC mismatch"));
        assert!(crc.source().is_none());
    }

    #[test]
    fn workflow_error_conversions() {
        let io_err = io::Error::new(io::ErrorKind::NotFound, "not found");
        let df_err = DatafileError::Open(io_err);

        // OpenError
        let open_err: OpenError = df_err.into();
        assert!(matches!(open_err, OpenError::FatalDatafileReplay(_)));
        let bitcask_err: BitcaskError = open_err.into();
        assert!(matches!(bitcask_err, BitcaskError::FatalOpenFailure(_)));
        assert!(bitcask_err.source().is_some());

        // GetError
        let get_err: GetError = GetError::DatafileMissing(123);
        assert!(format!("{get_err}").contains("123"));
        let bitcask_err: BitcaskError = get_err.into();
        assert!(matches!(
            bitcask_err,
            BitcaskError::GetFailure(GetError::DatafileMissing(123))
        ));

        // PutError
        let put_err: PutError = PutError::RecordLargerThanMaxDatafileSize(100);
        let bitcask_err: BitcaskError = put_err.into();
        assert!(matches!(bitcask_err, BitcaskError::PutFailure(_)));

        // RemoveError
        let remove_err: RemoveError = DatafileError::TornRecord.into();
        let bitcask_err: BitcaskError = remove_err.into();
        assert!(matches!(bitcask_err, BitcaskError::RemoveFailure(_)));

        // MergeError
        let merge_err: MergeError = HintfileError::CrcMismatch.into();
        let bitcask_err: BitcaskError = merge_err.into();
        assert!(matches!(bitcask_err, BitcaskError::FatalMergeFailure(_)));
    }

    #[test]
    fn auto_merge_error_conversions() {
        let merge_err = MergeError::FileCleanup(io::Error::other("cleanup failed"));
        let put_err: PutError = merge_err.into();
        assert!(matches!(put_err, PutError::AutoMerge(_)));

        let merge_err2 = MergeError::DirectoryScan(io::Error::other("scan failed"));
        let remove_err: RemoveError = merge_err2.into();
        assert!(matches!(remove_err, RemoveError::AutoMerge(_)));
    }
}
