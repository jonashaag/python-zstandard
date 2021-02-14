// Copyright (c) 2021-present, Gregory Szorc
// All rights reserved.
//
// This software may be modified and distributed under the terms
// of the BSD license. See the LICENSE file for details.

use {
    crate::{
        compression_dict::ZstdCompressionDict, decompression_reader::ZstdDecompressionReader,
        decompression_writer::ZstdDecompressionWriter, decompressionobj::ZstdDecompressionObj,
        decompressor_iterator::ZstdDecompressorIterator, exceptions::ZstdError,
    },
    pyo3::{
        buffer::PyBuffer,
        exceptions::{PyMemoryError, PyNotImplementedError, PyValueError},
        prelude::*,
        types::{PyBytes, PyList},
        wrap_pyfunction,
    },
    std::{marker::PhantomData, sync::Arc},
};

pub struct DCtx<'a>(*mut zstd_sys::ZSTD_DCtx, PhantomData<&'a ()>);

impl<'a> Drop for DCtx<'a> {
    fn drop(&mut self) {
        unsafe {
            zstd_sys::ZSTD_freeDCtx(self.0);
        }
    }
}

unsafe impl<'a> Send for DCtx<'a> {}
unsafe impl<'a> Sync for DCtx<'a> {}

impl<'a> DCtx<'a> {
    fn new() -> Result<Self, &'static str> {
        let dctx = unsafe { zstd_sys::ZSTD_createDCtx() };
        if dctx.is_null() {
            return Err("could not allocate ZSTD_DCtx instance");
        }

        Ok(Self(dctx, PhantomData))
    }

    pub fn dctx(&self) -> *mut zstd_sys::ZSTD_DCtx {
        self.0
    }

    pub fn memory_size(&self) -> usize {
        unsafe { zstd_sys::ZSTD_sizeof_DCtx(self.0) }
    }
}

#[pyclass(module = "zstandard.backend_rust")]
struct ZstdDecompressor {
    dict_data: Option<Py<ZstdCompressionDict>>,
    max_window_size: usize,
    format: zstd_sys::ZSTD_format_e,
    dctx: Arc<DCtx<'static>>,
}

impl ZstdDecompressor {
    fn setup_dctx(&self, py: Python, load_dict: bool) -> PyResult<()> {
        unsafe {
            zstd_sys::ZSTD_DCtx_reset(
                self.dctx.0,
                zstd_sys::ZSTD_ResetDirective::ZSTD_reset_session_only,
            );
        }

        if self.max_window_size != 0 {
            let zresult =
                unsafe { zstd_sys::ZSTD_DCtx_setMaxWindowSize(self.dctx.0, self.max_window_size) };
            if unsafe { zstd_sys::ZDICT_isError(zresult) } != 0 {
                return Err(ZstdError::new_err(format!(
                    "unable to set max window size: {}",
                    zstd_safe::get_error_name(zresult)
                )));
            }
        }

        let zresult = unsafe { zstd_sys::ZSTD_DCtx_setFormat(self.dctx.0, self.format) };
        if unsafe { zstd_sys::ZDICT_isError(zresult) } != 0 {
            return Err(ZstdError::new_err(format!(
                "unable to set decoding format: {}",
                zstd_safe::get_error_name(zresult)
            )));
        }

        if let Some(dict_data) = &self.dict_data {
            if load_dict {
                dict_data.try_borrow_mut(py)?.load_into_dctx(self.dctx.0)?;
            }
        }

        Ok(())
    }
}

#[pymethods]
impl ZstdDecompressor {
    #[new]
    #[args(dict_data = "None", max_window_size = "0", format = "0")]
    fn new(
        dict_data: Option<Py<ZstdCompressionDict>>,
        max_window_size: usize,
        format: u32,
    ) -> PyResult<Self> {
        let format = if format == zstd_sys::ZSTD_format_e::ZSTD_f_zstd1 as _ {
            zstd_sys::ZSTD_format_e::ZSTD_f_zstd1
        } else if format == zstd_sys::ZSTD_format_e::ZSTD_f_zstd1_magicless as _ {
            zstd_sys::ZSTD_format_e::ZSTD_f_zstd1_magicless
        } else {
            return Err(PyValueError::new_err(format!("invalid format value")));
        };

        let dctx = Arc::new(DCtx::new().map_err(|_| PyMemoryError::new_err(()))?);

        Ok(Self {
            dict_data,
            max_window_size,
            format,
            dctx,
        })
    }

    #[args(ifh, ofh, read_size = "None", write_size = "None")]
    fn copy_stream(
        &self,
        py: Python,
        ifh: &PyAny,
        ofh: &PyAny,
        read_size: Option<usize>,
        write_size: Option<usize>,
    ) -> PyResult<(usize, usize)> {
        let read_size = read_size.unwrap_or_else(|| zstd_safe::dstream_in_size());
        let write_size = write_size.unwrap_or_else(|| zstd_safe::dstream_out_size());

        if !ifh.hasattr("read")? {
            return Err(PyValueError::new_err(
                "first argument must have a read() method",
            ));
        }

        if !ofh.hasattr("write")? {
            return Err(PyValueError::new_err(
                "second argument must have a write() method",
            ));
        }

        self.setup_dctx(py, true)?;

        let mut dest_buffer: Vec<u8> = Vec::with_capacity(write_size);

        let mut in_buffer = zstd_sys::ZSTD_inBuffer {
            src: std::ptr::null(),
            size: 0,
            pos: 0,
        };

        let mut out_buffer = zstd_sys::ZSTD_outBuffer {
            dst: dest_buffer.as_mut_ptr() as *mut _,
            size: dest_buffer.capacity(),
            pos: 0,
        };

        let mut total_read = 0;
        let mut total_write = 0;

        // Read all available input.
        loop {
            let read_object = ifh.call_method1("read", (read_size,))?;
            let read_bytes: &PyBytes = read_object.downcast()?;
            let read_data = read_bytes.as_bytes();

            if read_data.len() == 0 {
                break;
            }

            total_read += read_data.len();

            in_buffer.src = read_data.as_ptr() as *const _;
            in_buffer.size = read_data.len();
            in_buffer.pos = 0;

            // Flush all read data to output.
            while in_buffer.pos < in_buffer.size {
                let zresult = unsafe {
                    zstd_sys::ZSTD_decompressStream(
                        self.dctx.0,
                        &mut out_buffer as *mut _,
                        &mut in_buffer as *mut _,
                    )
                };
                if unsafe { zstd_sys::ZSTD_isError(zresult) } != 0 {
                    return Err(ZstdError::new_err(format!(
                        "zstd decompressor error: {}",
                        zstd_safe::get_error_name(zresult)
                    )));
                }

                if out_buffer.pos != 0 {
                    unsafe {
                        dest_buffer.set_len(out_buffer.pos);
                    }

                    // TODO avoid buffer copy.
                    let data = PyBytes::new(py, &dest_buffer);

                    ofh.call_method1("write", (data,))?;
                    total_write += out_buffer.pos;
                    out_buffer.pos = 0;
                }
            }
            // Continue loop to keep reading.
        }

        Ok((total_read, total_write))
    }

    #[args(buffer, max_output_size = "0")]
    fn decompress<'p>(
        &mut self,
        py: Python<'p>,
        buffer: PyBuffer<u8>,
        max_output_size: usize,
    ) -> PyResult<&'p PyBytes> {
        self.setup_dctx(py, true)?;

        let output_size =
            unsafe { zstd_sys::ZSTD_getFrameContentSize(buffer.buf_ptr(), buffer.len_bytes()) };

        let (output_buffer_size, output_size) =
            if output_size == zstd_sys::ZSTD_CONTENTSIZE_ERROR as _ {
                return Err(ZstdError::new_err(
                    "error determining content size from frame header",
                ));
            } else if output_size == 0 {
                return Ok(PyBytes::new(py, &[]));
            } else if output_size == zstd_sys::ZSTD_CONTENTSIZE_UNKNOWN as _ {
                if max_output_size == 0 {
                    return Err(ZstdError::new_err(
                        "could not determine content size in frame header",
                    ));
                }

                (max_output_size, 0)
            } else {
                (output_size as _, output_size)
            };

        let mut dest_buffer: Vec<u8> = Vec::new();
        dest_buffer
            .try_reserve_exact(output_buffer_size)
            .map_err(|_| PyMemoryError::new_err(()))?;

        let mut out_buffer = zstd_sys::ZSTD_outBuffer {
            dst: dest_buffer.as_mut_ptr() as *mut _,
            size: dest_buffer.capacity(),
            pos: 0,
        };

        let mut in_buffer = zstd_sys::ZSTD_inBuffer {
            src: buffer.buf_ptr(),
            size: buffer.len_bytes(),
            pos: 0,
        };

        let zresult = unsafe {
            zstd_sys::ZSTD_decompressStream(
                self.dctx.0,
                &mut out_buffer as *mut _,
                &mut in_buffer as *mut _,
            )
        };
        if unsafe { zstd_sys::ZSTD_isError(zresult) } != 0 {
            Err(ZstdError::new_err(format!(
                "decompression error: {}",
                zstd_safe::get_error_name(zresult),
            )))
        } else if zresult != 0 {
            Err(ZstdError::new_err(
                "decompression error: did not decompress full frame",
            ))
        } else if output_size != 0 && out_buffer.pos != output_size as _ {
            Err(ZstdError::new_err(format!(
                "decompression error: decompressed {} bytes; expected {}",
                zresult, output_size
            )))
        } else {
            // TODO avoid memory copy
            unsafe { dest_buffer.set_len(out_buffer.pos) };
            Ok(PyBytes::new(py, &dest_buffer))
        }
    }

    fn decompress_content_dict_chain<'p>(
        &self,
        py: Python<'p>,
        frames: &PyList,
    ) -> PyResult<&'p PyBytes> {
        if frames.is_empty() {
            return Err(PyValueError::new_err("empty input chain"));
        }

        // First chunk should not be using a dictionary. We handle it specially.
        let chunk = frames.get_item(0);

        if !chunk.is_instance::<PyBytes>()? {
            return Err(PyValueError::new_err("chunk 0 must be bytes"));
        }

        let chunk_buffer: PyBuffer<u8> = PyBuffer::get(chunk)?;
        let mut params = zstd_sys::ZSTD_frameHeader {
            frameContentSize: 0,
            windowSize: 0,
            blockSizeMax: 0,
            frameType: zstd_sys::ZSTD_frameType_e::ZSTD_frame,
            headerSize: 0,
            dictID: 0,
            checksumFlag: 0,
        };
        let zresult = unsafe {
            zstd_sys::ZSTD_getFrameHeader(
                &mut params,
                chunk_buffer.buf_ptr() as *const _,
                chunk_buffer.len_bytes(),
            )
        };
        if unsafe { zstd_sys::ZSTD_isError(zresult) } != 0 {
            return Err(PyValueError::new_err("chunk 0 is not a valid zstd frame"));
        } else if zresult != 0 {
            return Err(PyValueError::new_err(
                "chunk 0 is too small to contain a zstd frame",
            ));
        }

        if params.frameContentSize == zstd_safe::CONTENTSIZE_UNKNOWN {
            return Err(PyValueError::new_err(
                "chunk 0 missing content size in frame",
            ));
        }

        self.setup_dctx(py, false)?;

        let mut last_buffer: Vec<u8> = Vec::with_capacity(params.frameContentSize as _);
        let mut out_buffer = zstd_sys::ZSTD_outBuffer {
            dst: last_buffer.as_mut_ptr() as *mut _,
            size: last_buffer.capacity(),
            pos: 0,
        };

        let mut in_buffer = zstd_sys::ZSTD_inBuffer {
            src: chunk_buffer.buf_ptr() as *mut _,
            size: chunk_buffer.len_bytes(),
            pos: 0,
        };

        let zresult = unsafe {
            zstd_sys::ZSTD_decompressStream(
                self.dctx.dctx(),
                &mut out_buffer as *mut _,
                &mut in_buffer as *mut _,
            )
        };
        if unsafe { zstd_sys::ZSTD_isError(zresult) } != 0 {
            return Err(ZstdError::new_err(format!(
                "could not decompress chunk 0: {}",
                zstd_safe::get_error_name(zresult)
            )));
        } else if zresult != 0 {
            return Err(ZstdError::new_err("chunk 0 did not decompress full frame"));
        }

        unsafe {
            last_buffer.set_len(out_buffer.pos);
        }

        // Special case of chain length 1.
        if frames.len() == 1 {
            // TODO avoid buffer copy.
            let chunk = PyBytes::new(py, &last_buffer);
            return Ok(chunk);
        }

        for (i, chunk) in frames.iter().enumerate().skip(1) {
            if !chunk.is_instance::<PyBytes>()? {
                return Err(PyValueError::new_err(format!("chunk {} must be bytes", i)));
            }

            let chunk_buffer: PyBuffer<u8> = PyBuffer::get(chunk)?;

            let zresult = unsafe {
                zstd_sys::ZSTD_getFrameHeader(
                    &mut params as *mut _,
                    chunk_buffer.buf_ptr(),
                    chunk_buffer.len_bytes(),
                )
            };
            if unsafe { zstd_sys::ZSTD_isError(zresult) } != 0 {
                return Err(PyValueError::new_err(format!(
                    "chunk {} is not a valid zstd frame",
                    i
                )));
            } else if zresult != 0 {
                return Err(PyValueError::new_err(format!(
                    "chunk {} is too small to contain a zstd frame",
                    i
                )));
            }

            if params.frameContentSize == zstd_safe::CONTENTSIZE_UNKNOWN {
                return Err(PyValueError::new_err(format!(
                    "chunk {} missing content size in frame",
                    i
                )));
            }

            let mut dest_buffer: Vec<u8> = Vec::with_capacity(params.frameContentSize as _);
            let mut out_buffer = zstd_sys::ZSTD_outBuffer {
                dst: dest_buffer.as_mut_ptr() as *mut _,
                size: dest_buffer.capacity(),
                pos: 0,
            };

            let mut in_buffer = zstd_sys::ZSTD_inBuffer {
                src: chunk_buffer.buf_ptr(),
                size: chunk_buffer.len_bytes(),
                pos: 0,
            };

            let zresult = unsafe {
                zstd_sys::ZSTD_decompressStream(
                    self.dctx.dctx(),
                    &mut out_buffer as *mut _,
                    &mut in_buffer as *mut _,
                )
            };
            if unsafe { zstd_sys::ZSTD_isError(zresult) } != 0 {
                return Err(ZstdError::new_err(format!(
                    "could not decompress chunk {}: {}",
                    i,
                    zstd_safe::get_error_name(zresult)
                )));
            } else if zresult != 0 {
                return Err(ZstdError::new_err(format!(
                    "chunk {} did not decompress full frame",
                    i
                )));
            }

            unsafe {
                dest_buffer.set_len(out_buffer.pos);
            }

            last_buffer = dest_buffer;
        }

        // TODO avoid buffer copy.
        Ok(PyBytes::new(py, &last_buffer))
    }

    #[args(write_size = "None")]
    fn decompressobj(
        &self,
        py: Python,
        write_size: Option<usize>,
    ) -> PyResult<ZstdDecompressionObj> {
        if let Some(write_size) = write_size {
            if write_size < 1 {
                return Err(PyValueError::new_err("write_size must be positive"));
            }
        }

        let write_size = write_size.unwrap_or_else(|| zstd_safe::dstream_out_size());

        self.setup_dctx(py, true)?;

        ZstdDecompressionObj::new(self.dctx.clone(), write_size)
    }

    fn memory_size(&self) -> PyResult<usize> {
        Ok(unsafe { zstd_sys::ZSTD_sizeof_DCtx(self.dctx.0) })
    }

    #[args(frames, decompressed_sizes = "None", threads = "0")]
    fn multi_decompress_to_buffer(
        &self,
        frames: &PyAny,
        decompressed_sizes: Option<&PyAny>,
        threads: usize,
    ) -> PyResult<()> {
        Err(PyNotImplementedError::new_err(()))
    }

    #[args(reader, read_size = "None", write_size = "None", skip_bytes = "None")]
    fn read_to_iter(
        &self,
        py: Python,
        reader: &PyAny,
        read_size: Option<usize>,
        write_size: Option<usize>,
        skip_bytes: Option<usize>,
    ) -> PyResult<ZstdDecompressorIterator> {
        let read_size = read_size.unwrap_or_else(|| zstd_safe::dstream_in_size());
        let write_size = write_size.unwrap_or_else(|| zstd_safe::dstream_out_size());
        let skip_bytes = skip_bytes.unwrap_or(0);

        if skip_bytes >= read_size {
            return Err(PyValueError::new_err(
                "skip_bytes must be smaller than read_size",
            ));
        }

        if !reader.hasattr("read")? && !reader.hasattr("__getitem__")? {
            return Err(PyValueError::new_err(
                "must pass an object with a read() method or conforms to buffer protocol",
            ));
        }

        self.setup_dctx(py, true)?;

        ZstdDecompressorIterator::new(
            py,
            self.dctx.clone(),
            reader,
            read_size,
            write_size,
            skip_bytes,
        )
    }

    #[args(
        source,
        read_size = "None",
        read_across_frames = "false",
        closefd = "true"
    )]
    fn stream_reader(
        &self,
        py: Python,
        source: &PyAny,
        read_size: Option<usize>,
        read_across_frames: bool,
        closefd: bool,
    ) -> PyResult<ZstdDecompressionReader> {
        let read_size = read_size.unwrap_or_else(|| zstd_safe::dstream_in_size());

        self.setup_dctx(py, true)?;

        ZstdDecompressionReader::new(
            py,
            self.dctx.clone(),
            source,
            read_size,
            read_across_frames,
            closefd,
        )
    }

    #[args(
        writer,
        write_size = "None",
        write_return_read = "true",
        closefd = "true"
    )]
    fn stream_writer(
        &self,
        py: Python,
        writer: &PyAny,
        write_size: Option<usize>,
        write_return_read: bool,
        closefd: bool,
    ) -> PyResult<ZstdDecompressionWriter> {
        let write_size = write_size.unwrap_or_else(|| zstd_safe::dstream_out_size());

        self.setup_dctx(py, true)?;

        ZstdDecompressionWriter::new(
            py,
            self.dctx.clone(),
            writer,
            write_size,
            write_return_read,
            closefd,
        )
    }
}

#[pyfunction]
fn estimate_decompression_context_size() -> usize {
    unsafe { zstd_sys::ZSTD_estimateDCtxSize() }
}

pub(crate) fn init_module(module: &PyModule) -> PyResult<()> {
    module.add_class::<ZstdDecompressor>()?;
    module.add_function(wrap_pyfunction!(
        estimate_decompression_context_size,
        module
    )?)?;

    Ok(())
}