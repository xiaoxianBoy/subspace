use parking_lot::Mutex;
use static_assertions::const_assert_eq;
use std::fs::{File, OpenOptions};
use std::io;
use std::io::{Seek, SeekFrom};
use std::path::Path;
use subspace_farmer_components::file_ext::FileExt;
#[cfg(windows)]
use subspace_farmer_components::file_ext::OpenOptionsExt;
use subspace_farmer_components::ReadAtSync;

/// 4096 is as a relatively safe size due to sector size on SSDs commonly being 512 or 4096 bytes
pub const DISK_SECTOR_SIZE: usize = 4096;
/// Restrict how much data to read from disk in a single call to avoid very large memory usage
const MAX_READ_SIZE: usize = 1024 * 1024;

const_assert_eq!(MAX_READ_SIZE % DISK_SECTOR_SIZE, 0);

/// Wrapper data structure for unbuffered I/O on Windows.
#[derive(Debug)]
pub struct UnbufferedIoFileWindows {
    file: File,
    physical_sector_size: usize,
    /// Scratch buffer of aligned memory for reads and writes
    scratch_buffer: Mutex<Vec<[u8; DISK_SECTOR_SIZE]>>,
}

impl ReadAtSync for UnbufferedIoFileWindows {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.read_exact_at(buf, offset)
    }
}

impl ReadAtSync for &UnbufferedIoFileWindows {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        (*self).read_at(buf, offset)
    }
}

impl FileExt for UnbufferedIoFileWindows {
    fn size(&mut self) -> io::Result<u64> {
        self.file.seek(SeekFrom::End(0))
    }

    fn preallocate(&mut self, len: u64) -> io::Result<()> {
        self.file.preallocate(len)
    }

    fn advise_random_access(&self) -> io::Result<()> {
        // Ignore, already set
        Ok(())
    }

    fn advise_sequential_access(&self) -> io::Result<()> {
        // Ignore, not supported
        Ok(())
    }

    fn read_exact_at(&self, buf: &mut [u8], mut offset: u64) -> io::Result<()> {
        if buf.is_empty() {
            return Ok(());
        }

        let mut scratch_buffer = self.scratch_buffer.lock();

        // First read up to `MAX_READ_SIZE - padding`
        let padding = (offset % self.physical_sector_size as u64) as usize;
        let first_unaligned_chunk_size = (MAX_READ_SIZE - padding).min(buf.len());
        let (unaligned_start, buf) = buf.split_at_mut(first_unaligned_chunk_size);
        {
            let bytes_to_read = unaligned_start.len();
            unaligned_start.copy_from_slice(self.read_exact_at_internal(
                &mut scratch_buffer,
                bytes_to_read,
                offset,
            )?);
            offset += unaligned_start.len() as u64;
        }

        if buf.is_empty() {
            return Ok(());
        }

        // Process the rest of the chunks, up to `MAX_READ_SIZE` at a time
        for buf in buf.chunks_mut(MAX_READ_SIZE) {
            let bytes_to_read = buf.len();
            buf.copy_from_slice(self.read_exact_at_internal(
                &mut scratch_buffer,
                bytes_to_read,
                offset,
            )?);
            offset += buf.len() as u64;
        }

        Ok(())
    }

    fn write_all_at(&self, buf: &[u8], mut offset: u64) -> io::Result<()> {
        if buf.is_empty() {
            return Ok(());
        }

        let mut scratch_buffer = self.scratch_buffer.lock();

        // First write up to `MAX_READ_SIZE - padding`
        let padding = (offset % self.physical_sector_size as u64) as usize;
        let first_unaligned_chunk_size = (MAX_READ_SIZE - padding).min(buf.len());
        let (unaligned_start, buf) = buf.split_at(first_unaligned_chunk_size);
        {
            self.write_all_at_internal(&mut scratch_buffer, unaligned_start, offset)?;
            offset += unaligned_start.len() as u64;
        }

        if buf.is_empty() {
            return Ok(());
        }

        // Process the rest of the chunks, up to `MAX_READ_SIZE` at a time
        for buf in buf.chunks(MAX_READ_SIZE) {
            self.write_all_at_internal(&mut scratch_buffer, buf, offset)?;
            offset += buf.len() as u64;
        }

        Ok(())
    }
}

impl UnbufferedIoFileWindows {
    /// Open file at specified path for random unbuffered access on Windows for reads to prevent
    /// huge memory usage (if file doesn't exist, it will be created).
    ///
    /// This abstraction is useless on other platforms and will just result in extra memory copies
    pub fn open(path: &Path) -> io::Result<Self> {
        let mut open_options = OpenOptions::new();
        #[cfg(windows)]
        open_options.advise_unbuffered();
        let file = open_options
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;

        // Physical sector size on many SSDs is smaller than 4096 and should improve performance
        let physical_sector_size = if file.read_at(&mut [0; 512], 512).is_ok() {
            512
        } else {
            DISK_SECTOR_SIZE
        };

        Ok(Self {
            file,
            physical_sector_size,
            // In many cases we'll want to read this much at once, so pre-allocate it right away
            scratch_buffer: Mutex::new(vec![
                [0; DISK_SECTOR_SIZE];
                MAX_READ_SIZE / DISK_SECTOR_SIZE
            ]),
        })
    }

    /// Truncates or extends the underlying file, updating the size of this file to become `size`.
    pub fn set_len(&self, size: u64) -> io::Result<()> {
        self.file.set_len(size)
    }

    fn read_exact_at_internal<'a>(
        &self,
        scratch_buffer: &'a mut Vec<[u8; DISK_SECTOR_SIZE]>,
        bytes_to_read: usize,
        offset: u64,
    ) -> io::Result<&'a [u8]> {
        // Make scratch buffer of a size that is necessary to read aligned memory, accounting
        // for extra bytes at the beginning and the end that will be thrown away
        let offset_in_buffer = (offset % DISK_SECTOR_SIZE as u64) as usize;
        let desired_buffer_size = (bytes_to_read + offset_in_buffer).div_ceil(DISK_SECTOR_SIZE);
        if scratch_buffer.len() < desired_buffer_size {
            scratch_buffer.resize(desired_buffer_size, [0; DISK_SECTOR_SIZE]);
        }

        // While buffer above is allocated with granularity of `MAX_DISK_SECTOR_SIZE`, reads are
        // done with granularity of physical sector size
        let offset_in_buffer = (offset % self.physical_sector_size as u64) as usize;
        self.file.read_exact_at(
            &mut scratch_buffer.flatten_mut()[..(bytes_to_read + offset_in_buffer)
                .div_ceil(self.physical_sector_size)
                * self.physical_sector_size],
            offset / self.physical_sector_size as u64 * self.physical_sector_size as u64,
        )?;

        Ok(&scratch_buffer.flatten()[offset_in_buffer..][..bytes_to_read])
    }

    /// Panics on writes over `MAX_READ_SIZE` (including padding on both ends)
    fn write_all_at_internal(
        &self,
        scratch_buffer: &mut Vec<[u8; DISK_SECTOR_SIZE]>,
        bytes_to_write: &[u8],
        offset: u64,
    ) -> io::Result<()> {
        // This is guaranteed by `UnbufferedIoFileWindows::open()`
        assert!(scratch_buffer.flatten().len() >= MAX_READ_SIZE);

        let aligned_offset =
            offset / self.physical_sector_size as u64 * self.physical_sector_size as u64;
        let padding = (offset - aligned_offset) as usize;
        // Calculate the size of the read including padding on both ends
        let bytes_to_read = (padding + bytes_to_write.len()).div_ceil(self.physical_sector_size)
            * self.physical_sector_size;

        if padding == 0 && bytes_to_read == bytes_to_write.len() {
            let scratch_buffer = &mut scratch_buffer.flatten_mut()[..bytes_to_read];
            scratch_buffer.copy_from_slice(bytes_to_write);
            self.file.write_all_at(scratch_buffer, offset)?;
        } else {
            // Read whole pages where `bytes_to_write` will be written
            self.read_exact_at_internal(scratch_buffer, bytes_to_read, aligned_offset)?;
            let scratch_buffer = &mut scratch_buffer.flatten_mut()[..bytes_to_read];
            // Update contents of existing pages and write into the file
            scratch_buffer[padding..][..bytes_to_write.len()].copy_from_slice(bytes_to_write);
            self.file.write_all_at(scratch_buffer, aligned_offset)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::single_disk_farm::unbuffered_io_file_windows::{
        UnbufferedIoFileWindows, MAX_READ_SIZE,
    };
    use rand::prelude::*;
    use std::fs;
    use subspace_farmer_components::file_ext::FileExt;
    use tempfile::tempdir;

    #[test]
    fn basic() {
        let tempdir = tempdir().unwrap();
        let file_path = tempdir.as_ref().join("file.bin");
        let mut data = vec![0u8; MAX_READ_SIZE * 5];
        thread_rng().fill(data.as_mut_slice());
        fs::write(&file_path, &data).unwrap();

        let mut file = UnbufferedIoFileWindows::open(&file_path).unwrap();

        for override_physical_sector_size in [None, Some(4096)] {
            if let Some(physical_sector_size) = override_physical_sector_size {
                file.physical_sector_size = physical_sector_size;
            }

            let mut buffer = Vec::new();
            for (offset, size) in [
                (0_usize, 512_usize),
                (0_usize, 4096_usize),
                (0, 500),
                (0, 4000),
                (5, 50),
                (12, 500),
                (96, 4000),
                (4000, 96),
                (10000, 5),
                (0, MAX_READ_SIZE),
                (0, MAX_READ_SIZE * 2),
                (5, MAX_READ_SIZE - 5),
                (5, MAX_READ_SIZE * 2 - 5),
                (5, MAX_READ_SIZE),
                (5, MAX_READ_SIZE * 2),
                (MAX_READ_SIZE, MAX_READ_SIZE),
                (MAX_READ_SIZE, MAX_READ_SIZE * 2),
                (MAX_READ_SIZE + 5, MAX_READ_SIZE - 5),
                (MAX_READ_SIZE + 5, MAX_READ_SIZE * 2 - 5),
                (MAX_READ_SIZE + 5, MAX_READ_SIZE),
                (MAX_READ_SIZE + 5, MAX_READ_SIZE * 2),
            ] {
                let data = &mut data[offset..][..size];
                buffer.resize(size, 0);
                // Read contents
                file.read_exact_at(buffer.as_mut_slice(), offset as u64)
                    .unwrap_or_else(|error| {
                        panic!(
                            "Offset {offset}, size {size}, override physical sector size \
                            {override_physical_sector_size:?}: {error}"
                        )
                    });

                // Ensure it is correct
                assert_eq!(
                    data,
                    buffer.as_slice(),
                    "Offset {offset}, size {size}, override physical sector size \
                    {override_physical_sector_size:?}"
                );

                // Update data with random contents and write
                thread_rng().fill(data);
                file.write_all_at(data, offset as u64)
                    .unwrap_or_else(|error| {
                        panic!(
                            "Offset {offset}, size {size}, override physical sector size \
                            {override_physical_sector_size:?}: {error}"
                        )
                    });

                // Read contents again
                file.read_exact_at(buffer.as_mut_slice(), offset as u64)
                    .unwrap_or_else(|error| {
                        panic!(
                            "Offset {offset}, size {size}, override physical sector size \
                            {override_physical_sector_size:?}: {error}"
                        )
                    });

                // Ensure it is correct too
                assert_eq!(
                    data,
                    buffer.as_slice(),
                    "Offset {offset}, size {size}, override physical sector size \
                    {override_physical_sector_size:?}"
                );
            }
        }
    }
}
