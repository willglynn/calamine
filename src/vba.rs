//! Parse vbaProject.bin file
//!
//! Retranscription from: 
//! https://github.com/unixfreak0037/officeparser/blob/master/officeparser.py

use zip::read::ZipFile;
use std::io::{Read, BufRead};
use std::collections::HashMap;
use std::cmp::{min, max};
use std::path::PathBuf;
use error::{ExcelResult, ExcelError};
use encoding::{Encoding, DecoderTrap};
use encoding::all::UTF_16LE;
use byteorder::{LittleEndian, ReadBytesExt};

const OLE_SIGNATURE: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
const ENDOFCHAIN: u32 = 0xFFFFFFFE;
const FREESECT: u32 = 0xFFFFFFFF;
const CLASS_EXTENSION: &'static str = "cls";
const MODULE_EXTENSION: &'static str = "bas";
const FORM_EXTENSION: &'static str = "frm";

#[allow(dead_code)]
pub struct VbaProject {
    header: Header,
    directories: Vec<Directory>,
    sectors: Sector,
    mini_sectors: Option<Sector>,
}

impl VbaProject {
    pub fn new(mut f: ZipFile) -> ExcelResult<VbaProject> {

        // load header
        debug!("loading header");
        let header = try!(Header::from_reader(&mut f));

        // check signature
        if header.ab_sig != OLE_SIGNATURE {
            return Err(ExcelError::Unexpected("invalid OLE signature (not an office document?)".to_string()));
        }

        let sector_size = 2u64.pow(header.sector_shift as u32) as usize;
        if (f.size() as usize - 512) % sector_size != 0 {
            return Err(ExcelError::Unexpected("last sector has invalid size".to_string()));
        }

        // Read whole file in memory (the file is delimited by sectors)
        let mut data = Vec::with_capacity(f.size() as usize - 512);
        try!(f.read_to_end(&mut data));
        let sector = Sector::new(data, sector_size);

        // load fat and dif sectors
        debug!("load dif");
        let mut fat_sectors = header.sect_fat.to_vec();
        let mut sector_id = header.sect_dif_start;
        while sector_id != FREESECT && sector_id != ENDOFCHAIN {
            fat_sectors.extend_from_slice(&try!(to_u32_vec(sector.get(sector_id))));
            sector_id = fat_sectors.pop().unwrap(); //TODO: check if in infinite loop
        }

        // load the FATs
        debug!("load fat");
        let mut fat = Vec::with_capacity(fat_sectors.len() * sector_size);
        for sector_id in fat_sectors.into_iter().filter(|id| *id != FREESECT) {
            fat.extend_from_slice(&try!(to_u32_vec(sector.get(sector_id))));
        }
        
        // set sector fats
        let sectors = sector.with_fats(fat);

        // get the list of directory sectors
        debug!("load dirs");
        let buffer = sectors.read_chain(header.sect_dir_start);
        let mut directories = Vec::with_capacity(buffer.len() / 128);
        for c in buffer.chunks(128) {
            directories.push(try!(Directory::from_slice(c)));
        }

        // load the mini streams
        let mini_sectors = if directories[0].sect_start == ENDOFCHAIN {
            None
        } else {
            debug!("load minis");
            let mut ministream = sectors.read_chain(directories[0].sect_start);
//             assert_eq!(ministream.len(), directories[0].ul_size as usize);
            ministream.truncate(directories[0].ul_size as usize); // should not be needed

            debug!("load minifat");
            let minifat = try!(to_u32_vec(&sectors.read_chain(header.sect_mini_fat_start)));

            let mini_sector_size = 2usize.pow(header.mini_sector_shift as u32);
            assert!(directories[0].ul_size as usize % mini_sector_size == 0);
            Some(Sector::new(ministream, mini_sector_size).with_fats(minifat))
        };

        Ok(VbaProject {
            header: header,
            directories: directories,
            sectors: sectors,
            mini_sectors: mini_sectors,
        })

    }

    pub fn get_stream(&self, name: &str) -> Option<Vec<u8>> {
        self.directories.iter()
            .find(|d| d.get_name().map(|n| &*n == name).unwrap_or(false))
            .map(|d| {
                let mut data = if d.ul_size < self.header.mini_sector_cutoff {
                    self.mini_sectors.as_ref()
                        .map_or_else(|| Vec::new(), |s| s.read_chain(d.sect_start))
                } else {
                    self.sectors.read_chain(d.sect_start)
                };
                data.truncate(d.ul_size as usize);
                data
            })
    }

    pub fn get_code_modules(&self) -> ExcelResult<HashMap<String, &'static str>> {
        let mut stream = &*match self.get_stream("PROJECT") {
            Some(s) => s,
            None => return Err(ExcelError::Unexpected("cannot find 'PROJECT' stream".to_string())),
        };
        
        let mut code_modules = HashMap::new();
        loop {
            let mut line = String::new();
            if try!(stream.read_line(&mut line)) == 0 { break; }
            let line = line.trim();
            if line.is_empty() || line.starts_with("[") { continue; }
            match line.find('=') {
                None => continue, // invalid or unknown PROJECT property line
                Some(pos) => {
                    let value = match &line[..pos] {
                        "Document" | "Class" => CLASS_EXTENSION,
                        "Module" => MODULE_EXTENSION,
                        "BaseClass" => FORM_EXTENSION,
                        _ => continue,
                    };
                    code_modules.insert(line[pos + 1..].to_string(), value);
                }
            }
        }
        Ok(code_modules)
    }

    pub fn read_vba(&self) -> ExcelResult<(Vec<Reference>, Vec<Module>)> {
        
        // dir stream
        let mut stream = &*match self.get_stream("dir") {
            Some(s) => try!(decompress_stream(&s)),
            None => return Err(ExcelError::Unexpected("cannot find 'dir' stream".to_string())),
        };

        // read header (not used)
        try!(self.read_dir_header(&mut stream));

        // array of REFERENCE records
        let references = try!(self.read_references(&mut stream));

        // modules
        let modules = try!(self.read_modules(&mut stream));
        Ok((references, modules))

    }

    fn read_dir_header(&self, stream: &mut &[u8]) -> ExcelResult<()> {

        // PROJECTSYSKIND Record
        let mut buf = [0; 12];
        try!(stream.read_exact(&mut buf[0..10]));
        
        // PROJECTLCID Record
        try!(stream.read_exact(&mut buf[0..10]));

        // PROJECTLCIDINVOKE Record
        try!(stream.read_exact(&mut buf[0..10]));

        // PROJECTCODEPAGE Record
        try!(stream.read_exact(&mut buf[..8]));

        // PROJECTNAME Record
        try!(stream.read_exact(&mut buf[..2]));
        let len = try!(stream.read_u32::<LittleEndian>()) as usize;
        try!(stream.read_exact(&mut vec![0; len])); // project name

        // PROJECTDOCSTRING Record
        try!(stream.read_exact(&mut buf[..2]));
        let len = try!(stream.read_u32::<LittleEndian>()) as usize;
        try!(stream.read_exact(&mut vec![0; len])); // project doc string
        try!(stream.read_exact(&mut buf[..2]));
        let len = try!(stream.read_u32::<LittleEndian>()) as usize;
        try!(stream.read_exact(&mut vec![0; len])); // project doc string unicode

        // PROJECTHELPFILEPATH Record - MS-OVBA 2.3.4.2.1.7
        try!(stream.read_exact(&mut buf[..2]));
        let len = try!(stream.read_u32::<LittleEndian>()) as usize;
        try!(stream.read_exact(&mut vec![0; len])); // project help file path - help file 1
        try!(stream.read_exact(&mut buf[..2]));
        let len = try!(stream.read_u32::<LittleEndian>()) as usize;
        try!(stream.read_exact(&mut vec![0; len])); // project help file path - help file 2

        // PROJECTHELPCONTEXT Record
        try!(stream.read_exact(&mut buf[..10]));

        // PROJECTLIBFLAGS Record
        try!(stream.read_exact(&mut buf[..10]));

        // PROJECTVERSION Record
        try!(stream.read_exact(&mut buf[..12]));

        // PROJECTCONSTANTS Record
        try!(stream.read_exact(&mut buf[..2]));
        let len = try!(stream.read_u32::<LittleEndian>()) as usize;
        try!(stream.read_exact(&mut vec![0; len])); // project constants - constants
        try!(stream.read_exact(&mut buf[..2]));
        let len = try!(stream.read_u32::<LittleEndian>()) as usize;
        try!(stream.read_exact(&mut vec![0; len])); // project constants - constants unicode

        Ok(())

    }

    fn read_references(&self, stream: &mut &[u8]) -> ExcelResult<Vec<Reference>> {

        let mut references = Vec::new();
        let mut buf = [0; 512];
        let mut reference = Reference { 
            name: "".to_string(), 
            description: "".to_string(), 
            path: "/".into() 
        };
        loop {

            let check = stream.read_u16::<LittleEndian>();
            match try!(check) {
                0x000F => {
                    if !reference.name.is_empty() { references.push(reference); }
                    break;
                },
                0x0016 => { 
                    if !reference.name.is_empty() { references.push(reference); }

                    // REFERENCENAME
                    let len = try!(stream.read_u32::<LittleEndian>()) as usize;
                    try!(stream.read_exact(&mut buf[..len])); // ref name

                    let name = try!(::std::string::String::from_utf8(buf[..len].into()));
                    reference = Reference {
                        name: name.clone(),
                        description: name.clone(),
                        path: "/".into(),
                    };

                    try!(stream.read_exact(&mut buf[..2]));
                    let len = try!(stream.read_u32::<LittleEndian>()) as usize;
                    try!(stream.read_exact(&mut buf[..len])); // ref name unicode
                },
                0x0033 => { 
                    // REFERENCEORIGINAL (followed by REFERENCECONTROL)
                    let len = try!(stream.read_u32::<LittleEndian>()) as usize;
                    try!(stream.read_exact(&mut buf[..len])); // ref original libid original
                    println!("original libid: {:?}", ::std::str::from_utf8(&buf[..len]));
                },
                0x002F => { 
                    // REFERENCECONTROL
                    try!(stream.read_exact(&mut buf[..4]));
                    let len = try!(stream.read_u32::<LittleEndian>()) as usize;
                    try!(stream.read_exact(&mut buf[..len])); // ref control libid twiddled
                    try!(stream.read_exact(&mut buf[..6]));
                    if try!(stream.read_u16::<LittleEndian>()) == 0x0016 {
                        let len = try!(stream.read_u32::<LittleEndian>()) as usize;
                        try!(stream.read_exact(&mut buf[..len])); // ref control name record extended
                        println!("ref control name: {:?}", ::std::str::from_utf8(&buf[..len]));

                        try!(stream.read_exact(&mut buf[..2]));
                        let len = try!(stream.read_u32::<LittleEndian>()) as usize;
                        try!(stream.read_exact(&mut buf[..len])); // ref control name unicode record extended
                        try!(stream.read_exact(&mut buf[..2]));
                    }
                    try!(stream.read_exact(&mut buf[..4]));
                    let len = try!(stream.read_u32::<LittleEndian>()) as usize;
                    try!(stream.read_exact(&mut buf[..len])); // ref control libid extended
                    try!(stream.read_exact(&mut buf[..26]));
                },
                0x000D => {
                    // REFERENCEREGISTERED
                    try!(stream.read_exact(&mut buf[..4]));
                    let len = try!(stream.read_u32::<LittleEndian>()) as usize;
                    try!(stream.read_exact(&mut buf[..len])); // ref registered libid
                    {
                        let registered_libid = try!(::std::str::from_utf8(&buf[..len]));
                        let mut registered_parts = registered_libid.split('#').rev();
                        
                        registered_parts.next().map(|p| reference.description = p.to_string());
                        registered_parts.next().map(|p| reference.path = p.into());
                    }
                    try!(stream.read_exact(&mut buf[..6]));
                },
                0x000E => {
                    // REFERENCEPROJECT
                    try!(stream.read_exact(&mut buf[..4]));
                    let len = try!(stream.read_u32::<LittleEndian>()) as usize;
                    try!(stream.read_exact(&mut buf[..len])); // ref project libid absolute
                    {
                        let absolute = try!(::std::str::from_utf8(&buf[..len]));
                        if absolute.starts_with("*\\C") {
                            reference.path = absolute[3..].into();
                        } else {
                            reference.path = absolute.into();
                        }
                    }
                    let len = try!(stream.read_u32::<LittleEndian>()) as usize;
                    try!(stream.read_exact(&mut buf[..len])); // ref project libid relative
                    try!(stream.read_exact(&mut buf[..6]));
                },
                c => return Err(ExcelError::Unexpected(format!("invalid of unknown check Id {}", c))),
            }
        }

        Ok(references)

    }

    fn read_modules(&self, stream: &mut &[u8]) -> ExcelResult<Vec<Module>> {
        let mut buf = [0; 512];
        try!(stream.read_exact(&mut buf[..4]));
        
        let module_len = try!(stream.read_u16::<LittleEndian>()) as usize;

        try!(stream.read_exact(&mut buf[..8]));
        let mut modules = Vec::with_capacity(module_len);

        for _ in 0..module_len {
            try!(stream.read_exact(&mut buf[..2]));

            let len = try!(stream.read_u32::<LittleEndian>()) as usize;
            try!(stream.read_exact(&mut buf[..len])); // ref name
            let name = try!(::std::string::String::from_utf8(buf[..len].to_vec()));
            let mut module = Module { name: name, ..Default::default() };

            loop {
                let section_id = try!(stream.read_u16::<LittleEndian>());
                match section_id {
                    0x0047 => {
                        let len = try!(stream.read_u32::<LittleEndian>()) as usize;
                        try!(stream.read_exact(&mut buf[..len])); // unicode name
                    },
                    0x001A => {
                        let len = try!(stream.read_u32::<LittleEndian>()) as usize;
                        try!(stream.read_exact(&mut buf[..len])); // stream name
                        module.stream_name = try!(::std::string::String::from_utf8(buf[..len].to_vec()));
                        try!(stream.read_exact(&mut buf[..2]));
                        let len = try!(stream.read_u32::<LittleEndian>()) as usize;
                        try!(stream.read_exact(&mut buf[..len])); // stream name unicode
                    },
                    0x001C => {
                        let len = try!(stream.read_u32::<LittleEndian>()) as usize;
                        try!(stream.read_exact(&mut buf[..len])); // doc string
                        try!(stream.read_exact(&mut buf[..2]));
                        let len = try!(stream.read_u32::<LittleEndian>()) as usize;
                        try!(stream.read_exact(&mut buf[..len])); // doc string unicode
                    },
                    0x0031 => {
                        try!(stream.read_exact(&mut buf[..4])); // offset
                        module.text_offset = try!(stream.read_u32::<LittleEndian>()) as usize;
                    },
                    0x001E => {
                        try!(stream.read_exact(&mut buf[..8])); // help context
                    },
                    0x002C => {
                        try!(stream.read_exact(&mut buf[..6])); // cookies
                    },
                    0x0021 | 0x0022 => {
                        try!(stream.read_exact(&mut buf[..4])); // reserved
                    },
                    0x0025 => {
                        try!(stream.read_exact(&mut buf[..4])); // read only
                    },
                    0x0028 => {
                        try!(stream.read_exact(&mut buf[..4])); // private
                    },
                    0x002B => {
                        try!(stream.read_exact(&mut buf[..4])); // private
                        break;
                    },
                    s => return Err(ExcelError::Unexpected(
                            format!("unknown or invalid module section id {}", s))),
                }
            }

            modules.push(module);
        }

        Ok(modules)
    }

    pub fn read_module(&self, module: &Module) -> ExcelResult<String> {
        match self.get_stream(&module.stream_name) {
            None => Err(ExcelError::Unexpected(format!("cannot find {} stream", module.stream_name))),
            Some(s) => {
                let data = try!(decompress_stream(&s[module.text_offset..]));
                let data = try!(::std::string::String::from_utf8(data));
                Ok(data)
            }
        }
    }

}

#[allow(dead_code)]
struct Header {
    ab_sig: [u8; 8],
    clid: [u8; 16],
    minor_version: u16,
    dll_version: u16,
    byte_order: u16,
    sector_shift: u16,
    mini_sector_shift: u16,
    reserved: u16,
    reserved1: u32,
    reserved2: u32,
    sect_fat_len: u32,
    sect_dir_start: u32,
    signature: u32,
    mini_sector_cutoff: u32,
    sect_mini_fat_start: u32,
    sect_mini_fat_len: u32,
    sect_dif_start: u32,
    sect_dif_len: u32,
    sect_fat: [u32; 109]
}

impl Header {
    fn from_reader<R: Read>(f: &mut R) -> ExcelResult<Header> {

        let mut ab_sig = [0; 8];
        try!(f.read_exact(&mut ab_sig));
        let mut clid = [0; 16];
        try!(f.read_exact(&mut clid));
        
        let minor_version = try!(f.read_u16::<LittleEndian>());
        let dll_version = try!(f.read_u16::<LittleEndian>());
        let byte_order = try!(f.read_u16::<LittleEndian>());
        let sector_shift = try!(f.read_u16::<LittleEndian>());
        let mini_sector_shift = try!(f.read_u16::<LittleEndian>());
        let reserved = try!(f.read_u16::<LittleEndian>());
        let reserved1 = try!(f.read_u32::<LittleEndian>());
        let reserved2 = try!(f.read_u32::<LittleEndian>());
        let sect_fat_len = try!(f.read_u32::<LittleEndian>());
        let sect_dir_start = try!(f.read_u32::<LittleEndian>());
        let signature = try!(f.read_u32::<LittleEndian>());
        let mini_sector_cutoff = try!(f.read_u32::<LittleEndian>());
        let sect_mini_fat_start = try!(f.read_u32::<LittleEndian>());
        let sect_mini_fat_len = try!(f.read_u32::<LittleEndian>());
        let sect_dif_start = try!(f.read_u32::<LittleEndian>());
        let sect_dif_len = try!(f.read_u32::<LittleEndian>());

        let mut sect_fat = [0u32; 109];
        for i in 0..109 {
            sect_fat[i] = try!(f.read_u32::<LittleEndian>());
        }

        Ok(Header {
            ab_sig: ab_sig, 
            clid: clid,
            minor_version: minor_version,
            dll_version: dll_version,
            byte_order: byte_order,
            sector_shift: sector_shift,
            mini_sector_shift: mini_sector_shift,
            reserved: reserved,
            reserved1: reserved1,
            reserved2: reserved2,
            sect_fat_len: sect_fat_len,
            sect_dir_start: sect_dir_start,
            signature: signature,
            mini_sector_cutoff: mini_sector_cutoff,
            sect_mini_fat_start: sect_mini_fat_start,
            sect_mini_fat_len: sect_mini_fat_len,
            sect_dif_start: sect_dif_start,
            sect_dif_len: sect_dif_len,
            sect_fat: sect_fat,
        })
    }
}

fn to_u32_vec(mut buffer: &[u8]) -> ExcelResult<Vec<u32>> {
    assert!(buffer.len() % 4 == 0);
    let mut res = Vec::with_capacity(buffer.len() / 4);
    for _ in 0..buffer.len() / 4 {
        res.push(try!(buffer.read_u32::<LittleEndian>()));
    }
    Ok(res)
}

fn decompress_stream(mut r: &[u8]) -> ExcelResult<Vec<u8>> {
    let mut res = Vec::new();

    if try!(r.read_u8()) != 1 {
        return Err(ExcelError::Unexpected("invalid signature byte".to_string()));
    }

    while !r.is_empty() {

        let compressed_chunk_header = try!(r.read_u16::<LittleEndian>());
        let chunk_is_compressed = (compressed_chunk_header & 0x8000) >> 15;

        if chunk_is_compressed == 0 { // uncompressed
            let len = res.len();
            res.extend_from_slice(&[0; 4096]);
            try!(r.read_exact(&mut res[len..]));
            continue;
        }

        let chunk_size = (compressed_chunk_header & 0x0FFF) + 3;
        let compressed_end = min(r.len() as u16, chunk_size);
        let decompressed_start = res.len();
        let mut compressed_current = 0;
        while compressed_current < compressed_end {
            let flag_byte = try!(r.read_u8());
            compressed_current += 1;

            for bit_index in 0..8 {
                if compressed_current >= compressed_end {
                    break;
                }

                if (1 << bit_index) & flag_byte == 0 { // Literal token
                    res.push(try!(r.read_u8()));
                    compressed_current += 1;
                } else {
                    // copy tokens
                    let copy_token = try!(r.read_u16::<LittleEndian>());
                    let difference = (res.len() - decompressed_start) as f64;
                    let bit_count = max(difference.log2().ceil() as u8, 4);
                    let len_mask = 0xFFFF >> bit_count;
                    let offset_mask = !len_mask;
                    let len = (copy_token & len_mask) + 3;
                    let temp1 = copy_token & offset_mask;
                    let temp2 = 16 - bit_count;
                    let offset = (temp1 >> temp2) + 1;
                    let copy_source = res.len() - offset as usize;
                    for i in 0..len as usize {
                        let val = res[copy_source + i];
                        res.push(val);
                    }
                    compressed_current += 2;
                }
            }

        }
    }
    Ok(res)
}

struct Sector {
    data: Vec<u8>,
    size: usize,
    fats: Vec<u32>,
}

impl Sector {

    fn new(data: Vec<u8>, size: usize) -> Sector {
        assert!(data.len() % size == 0);
        Sector {
            data: data,
            size: size as usize,
            fats: Vec::new(),
        }
    }

    fn with_fats(mut self, fats: Vec<u32>) -> Sector {
        self.fats = fats;
        self
    }

    fn get(&self, id: u32) -> &[u8] {
        &self.data[id as usize * self.size .. (id as usize + 1) * self.size]
    }

    fn read_chain(&self, mut sector_id: u32) -> Vec<u8> {
        let mut buffer = Vec::new();
        while sector_id != ENDOFCHAIN {
            buffer.extend_from_slice(self.get(sector_id));
            sector_id = self.fats[sector_id as usize];
        }
        buffer
    }

}

#[allow(dead_code)]
pub struct Directory {
    ab: [u8; 64],
    cb: u16,
    mse: i8,
    flags: i8,
    id_left_sib: u32,
    id_right_sib: u32,
    id_child: u32,
    cls_id: [u8; 16],
    dw_user_flags: u32,
    time: [u64; 2],
    sect_start: u32,
    ul_size: u32,
    dpt_prop_type: u16,
}

impl Directory {

    fn from_slice(mut rdr: &[u8]) -> ExcelResult<Directory> {
        let mut ab = [0; 64];
        try!(rdr.read_exact(&mut ab));

        let cb = try!(rdr.read_u16::<LittleEndian>());
        let mse = try!(rdr.read_i8());
        let flags = try!(rdr.read_i8());
        let id_left_sib = try!(rdr.read_u32::<LittleEndian>());
        let id_right_sib = try!(rdr.read_u32::<LittleEndian>());
        let id_child = try!(rdr.read_u32::<LittleEndian>());
        let mut cls_id = [0; 16];
        try!(rdr.read_exact(&mut cls_id));
        let dw_user_flags = try!(rdr.read_u32::<LittleEndian>());
        let time = [try!(rdr.read_u64::<LittleEndian>()),
                    try!(rdr.read_u64::<LittleEndian>())];
        let sect_start = try!(rdr.read_u32::<LittleEndian>());
        let ul_size = try!(rdr.read_u32::<LittleEndian>());
        let dpt_prop_type = try!(rdr.read_u16::<LittleEndian>());

        Ok(Directory {
            ab: ab,
            cb: cb,
            mse: mse,
            flags: flags,
            id_left_sib: id_left_sib,
            id_right_sib: id_right_sib,
            id_child: id_child,
            cls_id: cls_id,
            dw_user_flags: dw_user_flags,
            time: time,
            sect_start: sect_start,
            ul_size: ul_size,
            dpt_prop_type: dpt_prop_type,
        })

    }

    fn get_name(&self) -> ExcelResult<String> {
        let mut name = try!(UTF_16LE.decode(&self.ab, DecoderTrap::Ignore)
                            .map_err(ExcelError::Utf16));
        if let Some(len) = name.as_bytes().iter().position(|b| *b == 0) {
            name.truncate(len);
        }
        Ok(name)
    }
}

#[derive(Debug, Clone)]
pub struct Reference {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct Module {
    pub name: String,
    stream_name: String,
    text_offset: usize,
}

