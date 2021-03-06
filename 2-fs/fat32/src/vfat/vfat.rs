use std::io;
use std::io::Write;
use std::path::Path;

use util::SliceExt;
use mbr::MasterBootRecord;
use vfat::{Shared, Cluster, File, Dir, Entry, FatEntry, Error, Status};
use vfat::{BiosParameterBlock, CachedDevice, Partition};
use traits::{FileSystem, BlockDevice};

#[derive(Debug)]
pub struct VFat {
    device: CachedDevice,
    bytes_per_sector: u16,
    sectors_per_cluster: u8,
    sectors_per_fat: u32,
    fat_start_sector: u64,
    data_start_sector: u64,
    root_dir_cluster: Cluster,
}

impl VFat {
    pub fn from<T>(mut device: T) -> Result<Shared<VFat>, Error>
        where T: BlockDevice + 'static
    {
        let mbr = MasterBootRecord::from(&mut device)?;

        let partition = mbr.partition_at(0);
        if !partition.partition_type.is_fat() {
            return Err(Error::NotFound);
        }

        let partition_start = partition.relative_sector as u64;
        let ebpb = BiosParameterBlock::from(&mut device, partition_start)?;

        let bytes_per_sector = ebpb.bytes_per_sector();
        let cache = CachedDevice::new(
            device,
            Partition { start: partition_start,
                        sector_size: bytes_per_sector as u64 });

        let vfat = VFat {
            device: cache,
            bytes_per_sector,
            sectors_per_cluster: ebpb.sectors_per_cluster(),
            sectors_per_fat: ebpb.sectors_per_fat(),
            fat_start_sector: partition_start + ebpb.fat_start_sector(),
            data_start_sector: partition_start + ebpb.data_start_sector(),
            root_dir_cluster: Cluster::from(ebpb.root_cluster()),
        };

        Ok(Shared::new(vfat))
    }

    /// Read from an offset of a cluster into a buffer.
    pub fn read_cluster(&mut self, cluster: Cluster, offset: usize,
                        mut buf: &mut [u8])
        -> io::Result<usize>
    {
        let cluster_start_sector = self.data_start_sector
                                        + cluster.data_index() as u64
                                            * self.sectors_per_cluster as u64;
        let mut bytes_read: usize = 0;
        loop {
            let sector_index = (offset + bytes_read) as u64
                                / self.bytes_per_sector as u64;

            if sector_index >= self.sectors_per_cluster as u64 {
                break;
            } else {
                let byte_offset = (offset + bytes_read) as usize
                                    - sector_index as usize
                                        * self.bytes_per_sector as usize;
                let data = self.device.get(cluster_start_sector
                                            + sector_index)?;
                assert!(byte_offset < data.len());

                let bytes = buf.write(&data[byte_offset..])?;
                bytes_read += bytes;

                if buf.is_empty() {
                    break;
                }
            }
        }

        Ok(bytes_read)
    }

    fn append_cluster_data(&mut self, cluster: Cluster, vec: &mut Vec<u8>)
        -> io::Result<usize>
    {
        // To avoid extra copies, resize the vector to hold enough data for
        // the read.
        let cluster_size = self.bytes_per_sector as usize
                            * self.sectors_per_cluster as usize;
        vec.reserve(cluster_size);
        let len_before = vec.len();

        unsafe {
            vec.set_len(len_before + cluster_size);
        }

        let bytes_read = self.read_cluster(
            cluster, 0, &mut vec[len_before..])?;

        // Set the vector back to its actual size.
        unsafe {
            vec.set_len(len_before + bytes_read);
        }

        Ok(bytes_read)
    }

    pub fn find_sector(&mut self, start: Cluster, offset: usize)
        -> io::Result<(Cluster, usize)>
    {
        let cluster_size = self.bytes_per_sector as usize
                            * self.sectors_per_cluster as usize;

        let cluster_index = offset / cluster_size;
        let mut cluster = start;

        for i in 0..cluster_index {
            let fat_entry = self.fat_entry(cluster)?.status();

            match fat_entry {
                Status::Data(next) => {
                    cluster = next;
                },
                Status::Eoc(_) => {
                    if i + 1 != cluster_index {
                        return Err(io::Error::new(
                                                io::ErrorKind::UnexpectedEof,
                                                "Data does not match size"));
                    }

                    cluster = Cluster::from(0xFFFFFFFF);
                },
                _ => return Err(io::Error::new(io::ErrorKind::InvalidData,
                                               "Invalid cluster entry")),
            }
        }

        Ok((cluster, cluster_index * cluster_size))
    }

    /// Read all of the clusters chained from a starting cluster into a vector.
    pub fn read_chain(&mut self, start: Cluster, buf: &mut Vec<u8>)
        -> io::Result<usize>
    {
        let mut cluster = start;
        let mut bytes_read = 0;

        loop {
            let fat_entry = self.fat_entry(cluster)?.status();

            match fat_entry {
                Status::Data(next) => {
                    bytes_read += self.append_cluster_data(cluster, buf)?;
                    cluster = next;
                },
                Status::Eoc(_) => {
                    bytes_read += self.append_cluster_data(cluster, buf)?;
                    break;
                },
                _ => return Err(io::Error::new(io::ErrorKind::InvalidData,
                                               "Invalid cluster entry")),
            }
        }

        Ok(bytes_read)
    }

    /// Return a reference to a `FatEntry` for a cluster where the reference
    /// points directly into a cached sector.
    fn fat_entry(&mut self, cluster: Cluster) -> io::Result<&FatEntry> {
        // Map the cluster index to the FAT sector offset.
        let fat_sector = cluster.fat_index() * 4 / self.bytes_per_sector as u32;
        let fat_index: usize = (cluster.fat_index() * 4
                                - fat_sector * self.bytes_per_sector as u32)
                                as usize;
        if fat_sector >= self.sectors_per_fat {
            return Err(io::Error::new(io::ErrorKind::NotFound,
                                      "Invalid cluster index"));
        }

        let data = self.device.get(self.fat_start_sector + fat_sector as u64)?;
        Ok(unsafe { &data[fat_index..fat_index+4].cast()[0] })
    }
}

impl<'a> FileSystem for &'a Shared<VFat> {
    type File = File;
    type Dir = Dir;
    type Entry = Entry;

    fn open<P: AsRef<Path>>(self, path: P) -> io::Result<Self::Entry> {
        use vfat::{Entry, Metadata};
        use std::path::Component;

        let root_cluster = self.borrow().root_dir_cluster;
        let mut dir = Entry::new_dir("".to_string(), Metadata::default(),
                                     Dir::new(root_cluster, self.clone()));

        for segment in path.as_ref().components() {
            match segment {
                Component::ParentDir => {
                    use traits::Entry;
                    dir = dir.into_dir().ok_or(
                        io::Error::new(io::ErrorKind::NotFound,
                                       "Expected dir"))?.find("..")?;
                },
                Component::Normal(name) => {
                    use traits::Entry;
                    dir = dir.into_dir().ok_or(
                        io::Error::new(io::ErrorKind::NotFound,
                                       "Expected dir"))?.find(name)?;
                }
                _ => (),
            }
        }

        Ok(dir)
    }

    fn create_file<P: AsRef<Path>>(self, _path: P) -> io::Result<Self::File> {
        unimplemented!("read only file system")
    }

    fn create_dir<P>(self, _path: P, _parents: bool) -> io::Result<Self::Dir>
        where P: AsRef<Path>
    {
        unimplemented!("read only file system")
    }

    fn rename<P, Q>(self, _from: P, _to: Q) -> io::Result<()>
        where P: AsRef<Path>, Q: AsRef<Path>
    {
        unimplemented!("read only file system")
    }

    fn remove<P: AsRef<Path>>(self, _path: P, _children: bool) -> io::Result<()> {
        unimplemented!("read only file system")
    }
}

#[cfg(test)]
mod tests {
    use std::mem;
    use std::io::Cursor;
    use super::*;

    #[test]
    fn test_vfat_parse() {
        // MBR: 0-512
        // Partition 0: 1024-8192
        //  - EBPB: 1024-1536
        //  - FAT 1 2048-4096
        //  - DATA: 4096-8192
        static mut TEST_DATA: [u8; 8192] = [0u8; 8192];

        unsafe {
            let mbr = &mut TEST_DATA[..512];
            // Partition entries.
            let p1_offset = 446;
            mbr[p1_offset] = 0x80; // Bootable.
            mbr[p1_offset + 4] = 0xB; // Fat32.
             // Relative sector, 4-byte little endian so write to lowest byte.
            mbr[p1_offset + 8] = 2;
            // Signature.
            mbr[510] = 0x55;
            mbr[511] = 0xAA;
        }

        unsafe {
            let ebpb = &mut TEST_DATA[1024..1536];

            let bytes_per_sector: [u8; 2] = mem::transmute(1024u16);
            let sectors_per_cluster = 2u8;
            let reserved_sectors: [u8; 2] = mem::transmute(1u16);
            let fat_count = 1u8;
            let sectors_per_fat: [u8; 4] = mem::transmute(2u32);
            let root_cluster: [u8; 4] = mem::transmute(2u32);
            ebpb[11..13].copy_from_slice(&bytes_per_sector);
            ebpb[13] = sectors_per_cluster;
            ebpb[14..16].copy_from_slice(&reserved_sectors);
            ebpb[16] = fat_count;
            ebpb[36..40].copy_from_slice(&sectors_per_fat);
            ebpb[44..48].copy_from_slice(&root_cluster);

            // Signature.
            ebpb[510] = 0x55;
            ebpb[511] = 0xAA;
        }

        let vfat_shared = unsafe {
            VFat::from(Cursor::new(&mut TEST_DATA[..])).expect("Create VFat")
        };

        let vfat = vfat_shared.borrow();
        assert_eq!(vfat.bytes_per_sector, 1024);
        assert_eq!(vfat.sectors_per_cluster, 2);
        assert_eq!(vfat.sectors_per_fat, 2);
        assert_eq!(vfat.fat_start_sector, 3); // 2 physical + 1 logical.
        assert_eq!(vfat.data_start_sector, 5); // 2 physical + 3 logical.
    }
}
