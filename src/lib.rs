extern crate zip;
extern crate quick_xml;
extern crate encoding;
extern crate byteorder;

#[macro_use]
extern crate log;

mod error;
mod vba;

use std::path::Path;
use std::fs::File;
use std::io::BufReader;
use std::collections::HashMap;
use std::slice::Chunks;

use error::{ExcelError, ExcelResult};
use vba::VbaProject;

use zip::read::{ZipFile, ZipArchive};
use zip::result::ZipError;
use quick_xml::{XmlReader, Event, AsStr};

macro_rules! unexp {
    ($pat: expr) => {
        {
            return Err(ExcelError::Unexpected($pat.to_string()));
        }
    };
    ($pat: expr, $($args: expr)* ) => {
        {
            return Err(ExcelError::Unexpected(format!($pat, $($args)*)));
        }
    };
}

#[derive(Debug, Clone)]
pub enum DataType {
    Int(i64),
    Float(f64),
    String(String),
    Empty,
}

pub struct Excel {
    zip: ZipArchive<File>,
    strings: Vec<String>,
    /// Map of sheet names/sheet path within zip archive
    sheets: HashMap<String, String>,
}

#[derive(Debug, Default)]
pub struct Range {
    position: (u32, u32),
    size: (usize, usize),
    inner: Vec<DataType>,
}

/// An iterator to read `Range` struct row by row
pub struct Rows<'a> {
    inner: Chunks<'a, DataType>,
}

impl Excel {

    /// Opens a new workbook
    pub fn open<P: AsRef<Path>>(path: P) -> ExcelResult<Excel> {
        let f = try!(File::open(path));
        let zip = try!(ZipArchive::new(f));
        Ok(Excel { zip: zip, strings: vec![], sheets: HashMap::new() })
    }

    /// Does the workbook contain a vba project
    pub fn has_vba(&mut self) -> bool {
        self.zip.by_name("xl/vbaProject.bin").is_ok()
    }

    /// Gets vba project
    pub fn vba_project(&mut self) -> ExcelResult<VbaProject> {
        let f = try!(self.zip.by_name("xl/vbaProject.bin"));
        VbaProject::new(f)
    }

    /// Get all data from `Worksheet`
    pub fn worksheet_range(&mut self, name: &str) -> ExcelResult<Range> {
        try!(self.read_shared_strings());
        try!(self.read_sheets_names());
        let strings = &self.strings;
        let ws = match self.sheets.get(name) {
            Some(p) => try!(self.zip.by_name(p)),
            None => unexp!("Sheet '{}' does not exist", name),
        };
        Range::from_worksheet(ws, strings)
    }

    /// Loop through all archive files and opens 'xl/worksheets' files
    /// Store sheet name and path into self.sheets
    fn read_sheets_names(&mut self) -> ExcelResult<()> {
        if self.sheets.is_empty() {
            let sheets = {
                let mut sheets = HashMap::new();
                for i in 0..self.zip.len() {
                    let f = try!(self.zip.by_index(i));
                    let name = f.name().to_string();
                    if name.starts_with("xl/worksheets/") {
                        let xml = XmlReader::from_reader(BufReader::new(f))
                            .with_check(false)
                            .trim_text(false);
                        'xml_loop: for res_event in xml {
                            if let Ok(Event::Start(ref e)) = res_event {
                                if e.name() == b"sheetPr" {
                                    for a in e.attributes() {
                                        if let Ok((b"codeName", v)) = a {
                                            sheets.insert(try!(v.as_str()).to_string(), name);
                                            break 'xml_loop;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                sheets
            };
            self.sheets = sheets;
        }
        Ok(())
    }

    /// Read shared string list
    fn read_shared_strings(&mut self) -> ExcelResult<()> {
        if self.strings.is_empty() {
            match self.zip.by_name("xl/sharedStrings.xml") {
                Ok(f) => {
                    let mut xml = XmlReader::from_reader(BufReader::new(f))
                        .with_check(false)
                        .trim_text(false);

                    let mut strings = Vec::new();
                    while let Some(res_event) = xml.next() {
                        match res_event {
                            Ok(Event::Start(ref e)) if e.name() == b"t" => {
                                strings.push(try!(xml.read_text(b"t")));
                            }
                            Err(e) => return Err(ExcelError::Xml(e)),
                            _ => (),
                        }
                    }
                    self.strings = strings;
                },
                Err(ZipError::FileNotFound) => (),
                Err(e) => return Err(ExcelError::Zip(e)),
            }
        }

        Ok(())
    }

}

impl Range {

    /// open a xml `ZipFile` reader and read content of *sheetData* and *dimension* node
    fn from_worksheet(xml: ZipFile, strings: &[String]) -> ExcelResult<Range> {
        let mut xml = XmlReader::from_reader(BufReader::new(xml))
            .with_check(false)
            .trim_text(false);
        let mut data = Range::default();
        while let Some(res_event) = xml.next() {
            match res_event {
                Err(e) => return Err(ExcelError::Xml(e)),
                Ok(Event::Start(ref e)) => {
                    match e.name() {
                        b"dimension" => match e.attributes().filter_map(|a| a.ok())
                                .find(|&(key, _)| key == b"ref") {
                            Some((_, dim)) => {
                                let (position, size) = try!(get_dimension(try!(dim.as_str())));
                                data.position = position;
                                data.size = (size.0 as usize, size.1 as usize);
                                data.inner.reserve_exact(data.size.0 * data.size.1);
                            },
                            None => unexp!("Expecting dimension, got {:?}", e),
                        },
                        b"sheetData" => {
                            let _ = try!(data.read_sheet_data(&mut xml, strings));
                        }
                        _ => (),
                    }
                },
                _ => (),
            }
        }
        data.inner.shrink_to_fit();
        Ok(data)
    }
    
    /// get worksheet position (row, column)
    pub fn get_position(&self) -> (u32, u32) {
        self.position
    }

    /// get size
    pub fn get_size(&self) -> (usize, usize) {
        self.size
    }

    /// get cell value
    pub fn get_value(&self, i: usize, j: usize) -> &DataType {
        let idx = i * self.size.0 + j;
        &self.inner[idx]
    }

    /// get an iterator over inner rows
    pub fn rows(&self) -> Rows {
        let width = self.size.0;
        Rows { inner: self.inner.chunks(width) }
    }

    /// read sheetData node
    fn read_sheet_data(&mut self, xml: &mut XmlReader<BufReader<ZipFile>>, strings: &[String]) 
        -> ExcelResult<()> 
    {
        while let Some(res_event) = xml.next() {
            match res_event {
                Err(e) => return Err(ExcelError::Xml(e)),
                Ok(Event::Start(ref c_element)) => {
                    if c_element.name() == b"c" {
                        loop {
                            match xml.next() {
                                Some(Err(e)) => return Err(ExcelError::Xml(e)),
                                Some(Ok(Event::Start(ref e))) => {
                                    if e.name() == b"v" {
                                        let v = try!(xml.read_text(b"v"));
                                        let value = match c_element.attributes()
                                            .filter_map(|a| a.ok())
                                            .find(|&(k, _)| k == b"t") {
                                                Some((_, b"s")) => {
                                                    let idx: usize = try!(v.parse());
                                                    DataType::String(strings[idx].clone())
                                                },
                                                // TODO: check in styles to know which type is
                                                // supposed to be used
                                                _ => match v.parse() {
                                                    Ok(i) => DataType::Int(i),
                                                    Err(_) => try!(v.parse()
                                                                   .map(DataType::Float)),
                                                },
                                            };
                                        self.inner.push(value);
                                        break;
                                    } else {
                                        unexp!("not v node");
                                    }
                                },
                                Some(Ok(Event::End(ref e))) => {
                                    if e.name() == b"c" {
                                        self.inner.push(DataType::Empty);
                                        break;
                                    }
                                }
                                None => unexp!("End of xml"),
                                _ => (),
                            }
                        }
                    }
                },
                Ok(Event::End(ref e)) if e.name() == b"sheetData" => return Ok(()),
                _ => (),
            }
        }
        unexp!("Could not find </sheetData>")
    }

}

impl<'a> Iterator for Rows<'a> {
    type Item = &'a [DataType];
    fn next(&mut self) -> Option<&'a [DataType]> {
        self.inner.next()
    }
}

/// converts a text representation (e.g. "A6:G67") of a dimension into integers
/// - top left (row, column), 
/// - size (width, height)
fn get_dimension(dimension: &str) -> ExcelResult<((u32, u32), (u32, u32))> {
    match dimension.chars().position(|c| c == ':') {
        None => {
            get_row_column(dimension).map(|position| (position, (1, 1)))
        }, 
        Some(p) => {
            let top_left = try!(get_row_column(&dimension[..p]));
            let bottom_right = try!(get_row_column(&dimension[p + 1..]));
            Ok((top_left, (bottom_right.0 - top_left.0 + 1, bottom_right.1 - top_left.1 + 1)))
        }
    }
}

/// converts a text range name into its position (row, column)
fn get_row_column(range: &str) -> ExcelResult<(u32, u32)> {
    let mut col = 0;
    let mut pow = 1;
    let mut rowpos = range.len();
    let mut readrow = true;
    for c in range.chars().rev() {
        match c {
            '0'...'9' => {
                if readrow {
                    rowpos -= 1;
                } else {
                    unexp!("Numeric character are only allowed at the end of the range: {}", c);
                }
            }
            c @ 'A'...'Z' => {
                readrow = false;
                col += ((c as u8 - b'A') as u32 + 1) * pow;
                pow *= 26;
            },
            c @ 'a'...'z' => {
                readrow = false;
                col += ((c as u8 - b'a') as u32 + 1) * pow;
                pow *= 26;
            },
            _ => unexp!("Expecting alphanumeric character, got {:?}", c),
        }
    }
    let row = try!(range[rowpos..].parse());
    Ok((row, col))
}

#[cfg(test)]
mod tests {

    use super::Excel;

    #[test]
    fn test_range_sample() {
        let mut xl = Excel::open("/home/jtuffe/download/DailyValo_FX_Rates_Credit_05 25 16.xlsm")
            .expect("cannot open excel file");
        println!("{:?}", xl.sheets);
        let data = xl.worksheet_range("Sheet1");
        assert!(data.is_ok());
        for (i, r) in data.unwrap().rows().enumerate() {
            println!("Row {}: {:?}", i, r);
        }
    }
    
    #[test]
    fn test_vba() {
//         let path = "/home/jtuffe/download/test_vba.xlsm";
        let path = "/home/jtuffe/download/Extractions Simples.xlsb";
        let mut xl = Excel::open(path).expect("cannot open excel file");
        let vba_project = xl.vba_project().unwrap();
        let vba = vba_project.read_vba();
        assert!(vba.is_ok());
        let (references, modules) = vba.unwrap();
        println!("references: {:#?}", references);
        for module in &modules {
            let data = vba_project.read_module(module).unwrap();
            println!("module {}:\r\n{}", module.name, data);
        }

    }
}
