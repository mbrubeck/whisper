use memmap::{ Mmap, Protection };
use byteorder::{ BigEndian, WriteBytesExt };
use time;

mod header;
pub mod archive;

use self::header::{ Header, AggregationType };
use self::archive::Archive;

pub use self::header::STATIC_HEADER_SIZE;
pub use self::archive::ARCHIVE_INFO_SIZE;

use whisper::Point;
use whisper::Schema;

// Modules needed to create file on disk
use std::fs::OpenOptions;
extern crate libc;
use self::libc::ftruncate;
use std::os::unix::prelude::AsRawFd;
use std::io::{ self, Error};
use std::path::{ Path, PathBuf };
use std::fmt;
use std::cmp;
use std::iter::repeat;

pub struct WhisperFile {
	pub path: PathBuf,
	pub header: Header,
	pub archives: Vec< Archive >,
}

impl fmt::Debug for WhisperFile {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		try!(write!(f, "Meta data:
  aggregation method: {}
  max retention: {}
  xFilesFactor: {}

", self.header.aggregation_type, self.header.max_retention, self.header.x_files_factor));

		let mut index = 0;
		let mut offset = Header::archives_start(self.archives.len());

		let max_points = self.archives.iter().map(|x| x.points()).max().unwrap();
		let mut points_buf = Vec::with_capacity(max_points);

		for archive in &self.archives {
			try!(write!(f, "Archive {} info:
  offset: {}
  seconds per point: {}
  points: {}
  retention: {}
  size: {}

Archive {} data:
", index, offset, archive.seconds_per_point(), archive.points(), archive.seconds_per_point() * archive.points() as u32, archive.size(), index ));

			unsafe{ points_buf.set_len(archive.points()) };
			archive.read_points(archive.anchor_bucket_name(), &mut points_buf[..]);

			let mut points_index = 0;
			for point in &points_buf {
				try!(write!(f, "{}:	{},          {}\n", points_index, point.0, point.1));

				points_index = points_index + 1;
			}

			offset = offset + archive.size();
			index = index + 1;
		}

		write!(f,"")
	}
}


impl WhisperFile {
        fn new_transient(schema: &Schema) -> WhisperFile {
            let path = "/dev/null".into();
            let header = Header::new(AggregationType::Average, schema.max_retention(), 0.0);
            let archives = schema.retention_policies.iter().map(|policy| {
                Archive::new(
                    policy.precision,
                    policy.points() as usize,
                    Mmap::anonymous(policy.size_on_disk() as usize, Protection::ReadWrite).unwrap().into_view_sync()
                )
            }).collect();

            WhisperFile {
                path: path,
                header: header,
                archives: archives
            }
        }

	pub fn new<P>(path: P, schema: &Schema) -> io::Result<WhisperFile>
        where P: AsRef<Path> {
		let mut opened_file = try!(OpenOptions::new().read(true).write(true).create(true).open(path.as_ref()));

		// Allocate space on disk (could be costly!)
		{
			let size_needed = schema.size_on_disk();
			let raw_fd = opened_file.as_raw_fd();
			let retval = unsafe {
				// TODO skip to fallocate-like behavior. Will need wrapper for OSX.
				ftruncate(raw_fd, size_needed as i64)
			};
			if retval != 0 {
				return Err(Error::last_os_error());
			}
		}

		let xff = 0.5;
		let header = Header::new(AggregationType::Average, schema.max_retention(), xff);
		{
			try!( opened_file.write_u32::<BigEndian>( header.aggregation_type.to_u32() ));
			try!( opened_file.write_u32::<BigEndian>( header.max_retention ) );
			try!( opened_file.write_f32::<BigEndian>( header.x_files_factor ) );
			try!( opened_file.write_u32::<BigEndian>( schema.retention_policies.len() as u32 ) );
		}

		let mut archive_offset = Header::archives_start( schema.retention_policies.len() ) as u32;
		for retention_policy in &schema.retention_policies {
			try!( opened_file.write_u32::<BigEndian>( archive_offset as u32 ) );
			try!( opened_file.write_u32::<BigEndian>( retention_policy.precision ) );
			try!( opened_file.write_u32::<BigEndian>( retention_policy.points()  ) );

			archive_offset = archive_offset + retention_policy.size_on_disk();
		}

		let mmap = Mmap::open(&opened_file, Protection::ReadWrite ).unwrap();

		Ok( WhisperFile::open_mmap(path.as_ref(), mmap) )
	}

	// TODO: open should validate contents of whisper file
	// and return Result<WhisperFile, io::Error>
	pub fn open<P>(path: P) -> WhisperFile
        where P: AsRef<Path> {
		let mmap = Mmap::open_path(path.as_ref(), Protection::ReadWrite).unwrap();
		WhisperFile::open_mmap(path.as_ref(), mmap)
	}

	fn open_mmap<P>(path: P, mmap: Mmap) -> WhisperFile
	where P: AsRef<Path> {
		let mmap_view = mmap.into_view_sync();

		let header = {
			let slice = unsafe{ mmap_view.as_slice() };
			Header::new_from_slice(slice)
		};
		let archives = header.mmap_to_archives(mmap_view);

		let whisper_file = WhisperFile {
			path: path.as_ref().to_path_buf(),
			header: header,
			archives: archives
		};
		whisper_file
	}

        pub fn write(&mut self, point: &Point) {
            let now = time::get_time().sec;
            self._write(point, now)
        }

	fn _write(&mut self, point: &Point, now: i64) {
            let mut point = point.clone();
            let elapsed = now - point.0 as i64;

            enum WriteState {
              Initial,
              Aggregate(usize),
              Finished
            };

            (0..self.archives.len()).fold(WriteState::Initial, |state, index| {
                match state {
                  WriteState::Initial => {
                    if elapsed < 0 || elapsed as usize >= self.archives[index].retention() {
                      WriteState::Initial
                    } else {
                      self.archives[index].write(&point);
                      WriteState::Aggregate(index)
                    }
                  },

                  WriteState::Aggregate(last_index) => {
                    let (points, timestamp, ratio) = {
                      let seconds_per_point = self.archives[index].seconds_per_point();
                      let ref last_archive = self.archives[last_index];
                      let candidate_point_count = cmp::min((seconds_per_point / last_archive.seconds_per_point()) as usize, last_archive.points());
                      let timestamp = point.0 - (point.0 % seconds_per_point);
                      let from = archive::BucketName(timestamp);
                      let mut candidate_points: Vec<Point> = repeat(Point::default()).take(candidate_point_count).collect();
                      last_archive.read_points(from, &mut candidate_points).unwrap();
                      let points = candidate_points
                        .into_iter()
                        .enumerate()
                        .filter(|&(i, Point(t, v))| timestamp + (i as u32) * last_archive.seconds_per_point() == t)
                        .map(|(_, p)| p)
                        .collect::<Vec<Point>>();
                      let ratio = points.len() as f32 / candidate_point_count as f32;
                      (points, timestamp, ratio)
                    };

                    if ratio >= self.header.x_files_factor() {
                      point.0 = timestamp;
                      point.1 = self.header.aggregation_type().aggregate(&points);
                      self.archives[index].write(&point);
                      WriteState::Aggregate(index)
                    } else {
                      WriteState::Finished
                    }
                  },

                  WriteState::Finished => WriteState::Finished
                }
            });
	}

        fn read_all(&self) -> Vec<Vec<Point>> {
            self.archives.iter().map(|archive| {
                let mut points: Vec<Point> = repeat(Point::default()).take(archive.points()).collect();
                archive.read_points(archive.anchor_bucket_name(), &mut points).unwrap();
                points
            }).collect()
        }
}

#[cfg(test)]
mod tests {
	use whisper::{ Schema, WhisperFile, Point };
	use super::header;
        use super::time;

	use std::io::Cursor;
	use std::io::Write;
	use memmap::{ Mmap, Protection };

	// whisper-create.py blah.wsp 60:5
	// hexdump -v -e '"0x" 1/1 "%02X, "' blah.wsp
	const SAMPLE_FILE : [u8; 88] = [
	//  agg type
		0x00, 0x00, 0x00, 0x01,
	//  max ret
		0x00, 0x00, 0x01, 0x2C,
	// x_files_factor
		0x3F, 0x00, 0x00, 0x00,
	// archive_count
		0x00, 0x00, 0x00, 0x01,
	// archive_info[0].offset
		0x00, 0x00, 0x00, 0x1C,
	// archive_info[0].seconds_per_point
		0x00, 0x00, 0x00, 0x3C,
	// archive_info[0].points
		0x00, 0x00, 0x00, 0x05,
	// archive[0] data
		0x55, 0xD9, 0x33, 0xE8, 0x40, 0x59, 0x00, 0x00,
		0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
		0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
		0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
		0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
		0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
		0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
		0x00, 0x00, 0x00, 0x00
	];

	#[test]
	fn test_header(){
		let mut anon_mmap = Mmap::anonymous(SAMPLE_FILE.len(), Protection::ReadWrite).unwrap();
		{
			let slice : &mut [u8] = unsafe{ anon_mmap.as_mut_slice() };
			let mut cursor = Cursor::new(slice);
			cursor.write(&SAMPLE_FILE[..]).unwrap();
		};

		let hdr = header::Header::new_from_slice(unsafe{ anon_mmap.as_mut_slice() });

		assert_eq!(hdr.aggregation_type(), header::AggregationType::Average);
		assert_eq!(hdr.max_retention(), 300);
		assert_eq!(hdr.x_files_factor(), 0.5);

		let mmap_view = anon_mmap.into_view_sync();
		let archives = hdr.mmap_to_archives(mmap_view);
		assert_eq!(archives.len(), 1);
		assert_eq!(archives[0].seconds_per_point(), 60);
		assert_eq!(archives[0].points(), 5);
		assert_eq!(archives[0].size(), 60); // 5 points * (8 bytes float + 4 bytes ts) = 60 bytes
	}

	#[test]
	fn test_write() {
		let path = "/tmp/blah.wsp";
		let default_specs = vec!["1s:60s".to_string(), "1m:1y".to_string()];
		let schema = Schema::new_from_retention_specs(default_specs).unwrap();

		let mut file = WhisperFile::new(path, &schema).unwrap();

		file.write(&Point(10, 0.0))
	}

        /*
	#[test]
	fn test_write_aggregation() {
            let default_specs = vec!["1s:3s".to_string(), "1m:5m".to_string()];
            let schema = Schema::new_from_retention_specs(default_specs).unwrap();
            let mut file = WhisperFile::new_transient(&schema);

            file.write(&Point(1, 1.1));
            file.write(&Point(3, 3.1));
            file.write(&Point(9, 9.1));
            file.write(&Point(15, 15.1));
            file.write(&Point(65, 65.1));

            let result = file.read_all();
            assert_eq!(result, vec![vec![]]);
	}
        */

	#[test]
	fn test_read_all() {
            let default_specs = vec!["1s:60s".to_string(), "1m:5m".to_string()];
            let schema = Schema::new_from_retention_specs(default_specs).unwrap();
            let mut file = WhisperFile::new_transient(&schema);
            for &(t, v) in [
              (2, 1.1),
              (3, 3.1),
              (9, 9.1),
              (15, 15.1),
              (65, 65.1),
              (122, 122.1),
              (133, 133.1)
            ].iter() {
              file._write(&Point(1000 + t, v), 1000 + t as i64);
            }

            let result = file.read_all();
            assert_eq!(result, vec![vec![]]);
	}

	#[test]
	fn test_write_outside_retention(){

	}
}
