use std::path::Path;

use crate::{engine::BitcaskInstance, error::BitcaskError, options::BitcaskOptions};

pub trait KeyValueDatabase {
    type Options: Clone;
    type Error: std::error::Error;

    fn open(dir: &Path, options: Self::Options) -> Result<Self, Self::Error>
    where
        Self: Sized;

    fn get(&self, key: &[u8]) -> Result<Option<Box<[u8]>>, Self::Error>;
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), Self::Error>;
    fn remove(&mut self, key: &[u8]) -> Result<(), Self::Error>;

    fn merge(&mut self) -> Result<(), Self::Error>;
    fn sync(&mut self) -> Result<(), Self::Error>;

    fn len(&self) -> usize;
    fn is_empty(&self) -> bool;
}

impl KeyValueDatabase for BitcaskInstance {
    type Options = BitcaskOptions;

    type Error = BitcaskError;

    fn open(dir: &Path, options: Self::Options) -> Result<Self, Self::Error>
    where
        Self: Sized,
    {
        BitcaskInstance::open(dir, options)
    }

    fn get(&self, key: &[u8]) -> Result<Option<Box<[u8]>>, Self::Error> {
        self.get(key)
    }

    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), Self::Error> {
        self.put(key, value)
    }

    fn remove(&mut self, key: &[u8]) -> Result<(), Self::Error> {
        self.remove(key)
    }

    fn merge(&mut self) -> Result<(), Self::Error> {
        self.merge()
    }

    fn sync(&mut self) -> Result<(), Self::Error> {
        self.sync()
    }

    fn len(&self) -> usize {
        self.len()
    }

    fn is_empty(&self) -> bool {
        self.is_empty()
    }
}
