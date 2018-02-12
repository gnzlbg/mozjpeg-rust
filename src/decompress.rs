
extern crate libc;
extern crate mozjpeg_sys as ffi;

use libc::fdopen;
use marker::Marker;
use errormgr::ErrorMgr;
use errormgr::PanicingErrorMgr;
use component::CompInfoExt;
use component::CompInfo;
use colorspace::ColorSpaceExt;
use vec::VecUninitExtender;
use self::ffi::JPEG_LIB_VERSION;
use self::ffi::J_COLOR_SPACE as COLOR_SPACE;
use self::ffi::jpeg_decompress_struct;
use self::ffi::DCTSIZE;
use self::libc::{size_t, c_void, c_int, c_ulong, c_uchar};
use std::marker::PhantomData;
use std::slice;
use std::mem;
use std::ptr;
use std::cmp::min;
use std::os::unix::io::AsRawFd;
use std::fs::File;
use std::io;
use std::path::Path;

const MAX_MCU_HEIGHT: usize = 16;
const MAX_COMPONENTS: usize = 4;

pub const NO_MARKERS: &'static [Marker] = &[];
pub const ALL_MARKERS: &'static [Marker] = &[
    Marker::APP(0), Marker::APP(1), Marker::APP(2), Marker::APP(3), Marker::APP(4),
    Marker::APP(5), Marker::APP(6), Marker::APP(7), Marker::APP(8), Marker::APP(9),
    Marker::APP(10), Marker::APP(11), Marker::APP(12), Marker::APP(13), Marker::APP(14),
    Marker::COM,
];

pub struct DecompressConfig<'markers> {
    save_markers: &'markers [Marker],
    err: Option<ErrorMgr>
}

impl<'markers> DecompressConfig<'markers> {
    #[inline]
    pub fn new() -> Self {
        DecompressConfig {
            err: None,
            save_markers: NO_MARKERS,
        }
    }

    #[inline]
    fn create<'a>(self) -> Decompress<'a> {
        let mut d = Decompress::new_err(self.err.unwrap_or_else(|| <ErrorMgr as PanicingErrorMgr>::new()));
        for &marker in self.save_markers {
            d.save_marker(marker);
        }
        d
    }

    #[inline]
    pub fn with_err(mut self, err: ErrorMgr) -> Self {
        self.err = Some(err);
        self
    }

    #[inline]
    pub fn with_markers(mut self, save_markers: &'markers [Marker]) -> Self {
        self.save_markers = save_markers;
        self
    }

    #[inline]
    #[cfg(unix)]
    pub fn from_path<P: AsRef<Path>>(self, path: P) -> io::Result<Decompress<'static>> {
        self.from_file(File::open(path)?)
    }

    #[inline]
    #[cfg(unix)]
    pub fn from_file(self, file: File) -> io::Result<Decompress<'static>> {
        let mut d = self.create();
        d.set_file_src(Box::new(file))?;
        d.read_header()?;
        Ok(d)
    }

    #[inline]
    pub fn from_mem(self, mem: &[u8]) -> io::Result<Decompress> {
        let mut d = self.create();
        d.set_mem_src(mem);
        d.read_header()?;
        Ok(d)
    }
}

pub struct Decompress<'mem_src> {
    cinfo: jpeg_decompress_struct,
    own_error: Box<ErrorMgr>,
    own_file: Option<Box<File>>,
    _mem_marker: PhantomData<&'mem_src [u8]>, // Informs borrow checker that memory given in mem_src must outlive jpeg_decompress_struct
    _file_marker: PhantomData<&'mem_src mut File>,
}

pub struct MarkerData<'a> {
    pub marker: Marker,
    pub data: &'a [u8],
}

pub struct MarkerIter<'a> {
    marker_list: *mut ffi::jpeg_marker_struct,
    _uhh: ::std::marker::PhantomData<MarkerData<'a>>,
}

impl<'a> Iterator for MarkerIter<'a> {
    type Item = MarkerData<'a>;
    fn next(&mut self) -> Option<MarkerData<'a>> {
        if self.marker_list.is_null() {
            return None;
        }
        unsafe {
            let ref last = *self.marker_list;
            self.marker_list = last.next;
            Some(MarkerData {
                marker: last.marker.into(),
                data: ::std::slice::from_raw_parts(last.data, last.data_length as usize),
            })
        }
    }
}

impl<'mem_src> Decompress<'mem_src> {
    #[inline]
    pub fn with_err(err: ErrorMgr) -> DecompressConfig<'static> {
        Self::config().with_err(err)
    }

    #[inline]
    pub fn with_markers(save_markers: &[Marker]) -> DecompressConfig {
        Self::config().with_markers(save_markers)
    }

    #[inline]
    /// Decode file at path
    pub fn new_path<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        Self::config().from_path(path)
    }

    /// Decode an already-opened file
    #[inline]
    #[cfg(unix)]
    pub fn new_file(file: File) -> io::Result<Self> {
        Self::config().from_file(file)
    }

    #[inline]
    pub fn new_mem(mem: &'mem_src [u8]) -> io::Result<Self> {
        Self::config().from_mem(mem)
    }

    #[inline]
    fn config() -> DecompressConfig<'static> {
        DecompressConfig::new()
    }

    fn new_err(err: ErrorMgr) -> Self {
        unsafe {
            let mut newself = Decompress {
                cinfo: mem::zeroed(),
                own_error: Box::new(err),
                own_file: None,
                _mem_marker: PhantomData,
                _file_marker: PhantomData,
            };
            newself.cinfo.common.err = &mut *newself.own_error;

            let s = mem::size_of_val(&newself.cinfo) as size_t;
            ffi::jpeg_CreateDecompress(&mut newself.cinfo, JPEG_LIB_VERSION, s);

            newself
        }
    }

    pub fn components(&self) -> &[CompInfo] {

        unsafe {
            slice::from_raw_parts(self.cinfo.comp_info, self.cinfo.num_components as usize)
        }
    }

    pub fn components_mut(&mut self) -> &mut [CompInfo] {
        unsafe {
            slice::from_raw_parts_mut(self.cinfo.comp_info, self.cinfo.num_components as usize)
        }
    }

    #[cfg(unix)]
    fn set_file_src(&mut self, file: Box<File>) -> io::Result<()> {
        unsafe {
            let fh = fdopen(file.as_raw_fd(), b"rb".as_ptr() as *const i8);
            if fh.is_null() {
                return Err(io::Error::last_os_error());
            }
            ffi::jpeg_stdio_src(&mut self.cinfo, fh)
        }
        self.own_file = Some(file);
        Ok(())
    }

    fn set_mem_src(&mut self, file: &'mem_src [u8]) {
        unsafe {
            ffi::jpeg_mem_src(&mut self.cinfo, file.as_ptr(), file.len() as c_ulong);
        }
    }

    /// Result here is mostly useless, because it will panic if the file is invalid
    fn read_header(&mut self) -> io::Result<()> {
        let res = unsafe { ffi::jpeg_read_header(&mut self.cinfo, 0) };
        if res == 1 {
            return Ok(());
        } else {
            return Err(io::Error::new(io::ErrorKind::Other, format!("JPEG err {}", res)));
        }
    }

    pub fn color_space(&self) -> COLOR_SPACE {
        self.cinfo.jpeg_color_space
    }

    pub fn gamma(&self) -> f64 {
        self.cinfo.output_gamma
    }

    pub fn markers(&self) -> MarkerIter {
        MarkerIter {
            marker_list: self.cinfo.marker_list,
            _uhh: PhantomData,
        }
    }

    fn save_marker(&mut self, marker: Marker) {
        unsafe {
            ffi::jpeg_save_markers (&mut self.cinfo, marker.into(), 0xFFFF);
        }
    }

    /// width,height
    #[inline]
    pub fn size(&self) -> (usize, usize) {
        (self.width(), self.height())
    }

    #[inline]
    pub fn width(&self) -> usize {
        self.cinfo.image_width as usize
    }

    #[inline]
    pub fn height(&self) -> usize {
        self.cinfo.image_height as usize
    }

    fn set_raw_data_out(&mut self, raw: bool) {
        self.cinfo.raw_data_out = raw as ffi::boolean;
    }

    /// Start decompression with conversion to RGB
    pub fn rgb(mut self) -> io::Result<DecompressStarted<'mem_src>> {
        self.cinfo.out_color_space = ffi::J_COLOR_SPACE::JCS_RGB;
        return DecompressStarted::start_decompress(self);
    }

    pub fn raw(mut self) -> io::Result<DecompressStarted<'mem_src>> {
        self.set_raw_data_out(true);
        return DecompressStarted::start_decompress(self);
    }

    fn out_color_space(&self) -> COLOR_SPACE {
        self.cinfo.out_color_space
    }

    /// Start decompression without colorspace conversion
    pub fn image(self) -> io::Result<Format<'mem_src>> {
        use ffi::J_COLOR_SPACE::*;
        match self.out_color_space() {
            JCS_RGB => Ok(Format::RGB(DecompressStarted::start_decompress(self)?)),
            JCS_CMYK => Ok(Format::CMYK(DecompressStarted::start_decompress(self)?)),
            JCS_GRAYSCALE => Ok(Format::Gray(DecompressStarted::start_decompress(self)?)),
            format => Err(io::Error::new(io::ErrorKind::Other, format!("{:?}", format))),
        }
    }
}

pub enum Format<'a> {
    RGB(DecompressStarted<'a>),
    Gray(DecompressStarted<'a>),
    CMYK(DecompressStarted<'a>),
}

pub struct DecompressStarted<'mem_src> {
    dec: Decompress<'mem_src>,
}

impl<'mem_src> DecompressStarted<'mem_src> {
    fn start_decompress(mut dec: Decompress<'mem_src>) -> io::Result<Self> {
        let res = unsafe { ffi::jpeg_start_decompress(&mut dec.cinfo) };
        if 0 != res {
            Ok(DecompressStarted {
                dec
            })
        } else {
            Err(io::Error::new(io::ErrorKind::Other, format!("JPEG err {}", res)))
        }
    }

    fn out_color_space(&self) -> COLOR_SPACE {
        self.dec.out_color_space()
    }

    fn read_more_chunks(&self) -> bool {
        self.dec.cinfo.output_scanline < self.dec.cinfo.output_height
    }

    pub fn read_raw_data(&mut self, image_dest: &mut [&mut Vec<u8>]) {
        while self.read_more_chunks() {
            self.read_raw_data_chunk(image_dest);
        }
    }

    fn read_raw_data_chunk(&mut self, image_dest: &mut [&mut Vec<u8>]) {
        assert!(0 != self.dec.cinfo.raw_data_out, "Raw data not set");

        let mcu_height = self.dec.cinfo.max_v_samp_factor as usize * DCTSIZE;
        if mcu_height > MAX_MCU_HEIGHT {
            panic!("Subsampling factor too large");
        }

        let num_components = self.dec.components().len();
        if num_components > MAX_COMPONENTS || num_components > image_dest.len() {
            panic!("Too many components. Image has {}, destination vector has {} (max supported is {})", num_components, image_dest.len(), MAX_COMPONENTS);
        }

        unsafe {
            let mut row_ptrs = [[ptr::null_mut::<u8>(); MAX_MCU_HEIGHT]; MAX_COMPONENTS];
            let mut comp_ptrs = [ptr::null_mut::<*mut u8>(); MAX_COMPONENTS];
            for (ci, comp_info) in self.dec.components().iter().enumerate() {
                let row_stride = comp_info.row_stride();

                let comp_height = comp_info.v_samp_factor as usize * DCTSIZE;
                let original_len = image_dest[ci].len();
                image_dest[ci].extend_uninit(comp_height * row_stride);
                for ri in 0..comp_height {
                    let start = original_len + ri * row_stride;
                    row_ptrs[ci][ri] = (&mut image_dest[ci][start.. start + row_stride]).as_mut_ptr();
                }
                for ri in comp_height..mcu_height {
                    row_ptrs[ci][ri] = ptr::null_mut();
                }
                comp_ptrs[ci] = row_ptrs[ci].as_mut_ptr();
            }

            let lines_read = ffi::jpeg_read_raw_data(&mut self.dec.cinfo, comp_ptrs.as_mut_ptr(), mcu_height as u32) as usize;

            assert_eq!(lines_read, mcu_height); // Partial reads would make subsampled height tricky to define
        }
    }

    pub fn output_width(&self) -> usize {
        self.dec.cinfo.output_width as usize
    }

    pub fn output_height(&self) -> usize {
        self.dec.cinfo.output_height as usize
    }

    pub fn read_scanlines<T: Copy>(&mut self) -> Option<Vec<T>> {
        let num_components = self.out_color_space().num_components();
        assert_eq!(num_components, mem::size_of::<T>());
        let width = self.output_width();
        let height = self.output_height();
        let mut image_dst:Vec<T> = Vec::with_capacity(self.output_height() * width);
        unsafe {
            image_dst.extend_uninit(height * width);

            while self.read_more_chunks() {
                let start_line = self.dec.cinfo.output_scanline as usize;
                let rest:&mut [T] = &mut image_dst[width * start_line ..];
                let rows = (&mut rest.as_mut_ptr()) as *mut *mut T;

                let rows_read = ffi::jpeg_read_scanlines(&mut self.dec.cinfo, rows as *mut *mut u8, 1) as usize;
                debug_assert_eq!(start_line + rows_read, self.dec.cinfo.output_scanline as usize, "wat {}/{} at {}", rows_read, height, start_line);
                if 0 == rows_read {
                    return None;
                }
            }
        }
        return Some(image_dst);
    }

    pub fn components(&self) -> &[CompInfo] {
        self.dec.components()
    }

    pub fn components_mut(&mut self) -> &[CompInfo] {
        self.dec.components_mut()
    }

    pub fn finish_decompress(mut self) -> bool {
        unsafe {
            0 != ffi::jpeg_finish_decompress(&mut self.dec.cinfo)
        }
    }
}


impl<'mem_src> Drop for Decompress<'mem_src> {
    fn drop(&mut self) {
        unsafe {
            ffi::jpeg_destroy_decompress(&mut self.cinfo);
        }
    }
}

#[test]
fn read_file() {
    use std::fs::File;
    use std::io::Read;
    use colorspace::ColorSpace;
    use colorspace::ColorSpaceExt;

    let mut data = Vec::new();
    File::open("tests/test.jpg").unwrap().read_to_end(&mut data).unwrap();
    assert_eq!(2169, data.len());

    let dinfo = Decompress::new_mem(&data[..]).unwrap();


    assert_eq!(1.0, dinfo.gamma());
    assert_eq!(ColorSpace::JCS_YCbCr, dinfo.color_space());
    assert_eq!(dinfo.components().len(), dinfo.color_space().num_components() as usize);


    assert_eq!((45, 30), dinfo.size());
    {
        let comps = dinfo.components();
        assert_eq!(2, comps[0].h_samp_factor);
        assert_eq!(2, comps[0].v_samp_factor);

        assert_eq!(48, comps[0].row_stride());
        assert_eq!(32, comps[0].col_stride());

        assert_eq!(1, comps[1].h_samp_factor);
        assert_eq!(1, comps[1].v_samp_factor);
        assert_eq!(1, comps[2].h_samp_factor);
        assert_eq!(1, comps[2].v_samp_factor);

        assert_eq!(24, comps[1].row_stride());
        assert_eq!(16, comps[1].col_stride());
        assert_eq!(24, comps[2].row_stride());
        assert_eq!(16, comps[2].col_stride());
    }

    let mut dinfo = dinfo.raw().unwrap();

    let mut has_chunks = false;
    let mut bitmaps = [&mut Vec::new(), &mut Vec::new(), &mut Vec::new()];
    while dinfo.read_more_chunks() {
        has_chunks = true;
        dinfo.read_raw_data_chunk(&mut bitmaps);
        assert_eq!(bitmaps[0].len(), 4*bitmaps[1].len());
    }
    assert!(has_chunks);

    for (bitmap, comp) in bitmaps.iter().zip(dinfo.components()) {
        assert_eq!(comp.row_stride() * comp.col_stride(), bitmap.len());
    }

    assert!(dinfo.finish_decompress());
}

#[test]
fn no_markers() {
    use std::fs::File;
    use std::io::Read;
    use colorspace::ColorSpace;
    use colorspace::ColorSpaceExt;

    let dinfo = Decompress::new_path("tests/test.jpg").unwrap();
    assert_eq!(0, dinfo.markers().count());

    let dinfo = Decompress::with_markers(&[]).from_path("tests/test.jpg").unwrap();
    assert_eq!(0, dinfo.markers().count());
}

#[test]
fn read_file_rgb() {
    use std::fs::File;
    use std::io::Read;
    use colorspace::ColorSpace;
    use colorspace::ColorSpaceExt;

    let mut data = Vec::new();
    File::open("tests/test.jpg").unwrap().read_to_end(&mut data).unwrap();
    let dinfo = Decompress::with_markers(ALL_MARKERS).from_mem(&data[..]).unwrap();

    assert_eq!(ColorSpace::JCS_YCbCr, dinfo.color_space());

    assert_eq!(1, dinfo.markers().count());

    let mut dinfo = dinfo.rgb().unwrap();
    assert_eq!(ColorSpace::JCS_RGB, dinfo.out_color_space());
    assert_eq!(dinfo.components().len(), dinfo.out_color_space().num_components() as usize);

    let bitmap:Vec<(u8,u8,u8)> = dinfo.read_scanlines().unwrap();
    assert_eq!(bitmap.len(), 45*30);

    assert!(!bitmap.contains(&(0,0,0)));

    assert!(dinfo.finish_decompress());
}