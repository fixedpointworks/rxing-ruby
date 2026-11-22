use magnus::{function, method, prelude::*, Error, RArray, RString, Ruby};
use std::{
    ffi::c_void,
    panic::{self, AssertUnwindSafe},
};

#[magnus::wrap(class = "Rxing::BarcodeResult")]
struct BarcodeResult {
    text: String,
    format: String,
    points: Vec<(i32, i32)>,
}

impl BarcodeResult {
    fn new(text: String, format: String, points: Vec<(i32, i32)>) -> Self {
        Self { text, format, points }
    }

    fn text(&self) -> &str {
        &self.text
    }

    fn format(&self) -> &str {
        &self.format
    }

    fn points(ruby: &Ruby, rb_self: &Self) -> Result<RArray, Error> {
        let array = ruby.ary_new();
        for (x, y) in &rb_self.points {
            let point = ruby.ary_new();
            point.push(*x)?;
            point.push(*y)?;
            array.push(point)?;
        }
        Ok(array)
    }
}

#[magnus::wrap(class = "Rxing::BarcodeReader")]
struct BarcodeReader;

impl BarcodeReader {
    fn new() -> Self {
        BarcodeReader
    }

    fn read_from_file(ruby: &Ruby, _rb_self: &Self, path: String) -> Result<RArray, Error> {
        let results = without_gvl(ruby, move || {
            rxing::helpers::detect_multiple_in_file(&path).map_err(|e| format!("Failed to read file: {}", e))
        })?;
        to_array(ruby, results)
    }

    fn read_from_bytes(ruby: &Ruby, _rb_self: &Self, data: RString) -> Result<RArray, Error> {
        // The bytes are copied because the Ruby string may change once the GVL is released.
        let bytes = unsafe { data.as_slice() }.to_vec();
        let results = without_gvl(ruby, move || {
            let luma = image::load_from_memory(&bytes)
                .map_err(|e| format!("Failed to load image: {}", e))?
                .to_luma8();
            let (width, height) = luma.dimensions();
            rxing::helpers::detect_multiple_in_luma(luma.into_raw(), width, height)
                .map_err(|e| format!("Failed to detect barcodes: {}", e))
        })?;
        to_array(ruby, results)
    }
}

fn to_array(ruby: &Ruby, results: Vec<rxing::RXingResult>) -> Result<RArray, Error> {
    let array = ruby.ary_new();
    for result in results {
        let points = result.getPoints();
        if points.len() >= 2 {
            let points = points.iter().map(|p| (p.x as i32, p.y as i32)).collect();
            array.push(BarcodeResult::new(
                result.getText().to_string(),
                format!("{:?}", result.getBarcodeFormat()),
                points,
            ))?;
        }
    }
    Ok(array)
}

type Detection = Result<Vec<rxing::RXingResult>, String>;

struct Call<F> {
    work: Option<F>,
    result: Option<Detection>,
}

unsafe extern "C" fn run<F: FnOnce() -> Detection>(data: *mut c_void) -> *mut c_void {
    let call = unsafe { &mut *(data as *mut Call<F>) };
    if let Some(work) = call.work.take() {
        // A panic must not unwind into Ruby's C frames.
        call.result = Some(
            panic::catch_unwind(AssertUnwindSafe(work))
                .unwrap_or_else(|_| Err("Barcode detection panicked".to_string())),
        );
    }
    std::ptr::null_mut()
}

// Decoding a large scan can take seconds. Holding the GVL that long stops every other thread in the
// process, including Solid Queue's heartbeat and signal handling. The work touches no Ruby object.
// Without an unblock function a running decode cannot be interrupted, only waited for.
fn without_gvl<F: FnOnce() -> Detection>(ruby: &Ruby, work: F) -> Result<Vec<rxing::RXingResult>, Error> {
    let mut call = Call { work: Some(work), result: None };
    let data = &mut call as *mut Call<F> as *mut c_void;
    // rb_thread_call_without_gvl may raise a pending interrupt before it runs the work; protect turns
    // that into an Error instead of a longjmp through these Rust frames.
    magnus::rb_sys::protect(|| unsafe {
        rb_sys::rb_thread_call_without_gvl(Some(run::<F>), data, None, std::ptr::null_mut());
        rb_sys::Qnil as rb_sys::VALUE
    })?;
    match call.result {
        Some(result) => result.map_err(|message| Error::new(ruby.exception_runtime_error(), message)),
        None => Err(Error::new(ruby.exception_runtime_error(), "Barcode detection was interrupted")),
    }
}

#[magnus::init]
fn init(ruby: &Ruby) -> Result<(), Error> {
    let module = ruby.define_module("Rxing")?;

    let reader_class = module.define_class("BarcodeReader", ruby.class_object())?;
    reader_class.define_singleton_method("new", function!(BarcodeReader::new, 0))?;
    reader_class.define_method("read_from_file", method!(BarcodeReader::read_from_file, 1))?;
    reader_class.define_method("read_from_bytes", method!(BarcodeReader::read_from_bytes, 1))?;

    let result_class = module.define_class("BarcodeResult", ruby.class_object())?;
    result_class.define_method("text", method!(BarcodeResult::text, 0))?;
    result_class.define_method("format", method!(BarcodeResult::format, 0))?;
    result_class.define_method("points", method!(BarcodeResult::points, 0))?;

    Ok(())
}
