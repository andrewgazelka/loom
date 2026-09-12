use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::Path;

#[derive(Default)]
pub struct Preimages {
    pub items: BTreeMap<String, Vec<u8>>,
    pub cycles: BTreeMap<String, Vec<u8>>,
}

impl Preimages {
    pub fn write(&self, directory: &Path) -> io::Result<()> {
        write_objects(directory, &self.items)?;
        if !self.cycles.is_empty() {
            write_objects(&directory.join("cycles"), &self.cycles)?;
        }
        Ok(())
    }
}

fn write_objects(directory: &Path, objects: &BTreeMap<String, Vec<u8>>) -> io::Result<()> {
    std::fs::create_dir_all(directory).map_err(|error| {
        io::Error::other(format!("cannot create {}: {error}", directory.display()))
    })?;
    for (hash, bytes) in objects {
        let path = directory.join(hash);
        let result = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                let result = file.write_all(bytes);
                if result.is_err() {
                    std::fs::remove_file(&path)?;
                }
                result
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let existing = std::fs::read(&path)?;
                if existing == *bytes {
                    Ok(())
                } else {
                    Err(io::Error::other("existing preimage has different bytes"))
                }
            }
            Err(error) => Err(error),
        };
        result.map_err(|error| {
            io::Error::other(format!("cannot write preimage {}: {error}", path.display()))
        })?;
    }
    Ok(())
}
