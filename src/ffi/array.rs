//! Contains functionality to load an ArrayData from the C Data Interface
use std::ptr::NonNull;
use std::sync::Arc;

use crate::{
    array::*,
    bitmap::{utils::bytes_for, Bitmap},
    buffer::{bytes::Bytes, Buffer},
    datatypes::{DataType, PhysicalType},
    error::{Error, Result},
    ffi::schema::get_child,
    types::NativeType,
};

use super::ArrowArray;

/// Reads a valid `ffi` interface into a `Box<dyn Array>`
/// # Errors
/// If and only if:
/// * the interface is not valid (e.g. a null pointer)
pub unsafe fn try_from<A: ArrowArrayRef>(array: A) -> Result<Box<dyn Array>> {
    use PhysicalType::*;
    Ok(match array.data_type().to_physical_type() {
        Null => Box::new(NullArray::try_from_ffi(array)?),
        Boolean => Box::new(BooleanArray::try_from_ffi(array)?),
        Primitive(primitive) => with_match_primitive_type!(primitive, |$T| {
            Box::new(PrimitiveArray::<$T>::try_from_ffi(array)?)
        }),
        Utf8 => Box::new(Utf8Array::<i32>::try_from_ffi(array)?),
        LargeUtf8 => Box::new(Utf8Array::<i64>::try_from_ffi(array)?),
        Binary => Box::new(BinaryArray::<i32>::try_from_ffi(array)?),
        LargeBinary => Box::new(BinaryArray::<i64>::try_from_ffi(array)?),
        FixedSizeBinary => Box::new(FixedSizeBinaryArray::try_from_ffi(array)?),
        List => Box::new(ListArray::<i32>::try_from_ffi(array)?),
        LargeList => Box::new(ListArray::<i64>::try_from_ffi(array)?),
        FixedSizeList => Box::new(FixedSizeListArray::try_from_ffi(array)?),
        Struct => Box::new(StructArray::try_from_ffi(array)?),
        Dictionary(key_type) => {
            match_integer_type!(key_type, |$T| {
                Box::new(DictionaryArray::<$T>::try_from_ffi(array)?)
            })
        }
        Union => Box::new(UnionArray::try_from_ffi(array)?),
        Map => Box::new(MapArray::try_from_ffi(array)?),
    })
}

// Sound because the arrow specification does not allow multiple implementations
// to change this struct
// This is intrinsically impossible to prove because the implementations agree
// on this as part of the Arrow specification
unsafe impl Send for ArrowArray {}
unsafe impl Sync for ArrowArray {}

impl Drop for ArrowArray {
    fn drop(&mut self) {
        match self.release {
            None => (),
            Some(release) => unsafe { release(self) },
        };
    }
}

// callback used to drop [ArrowArray] when it is exported
unsafe extern "C" fn c_release_array(array: *mut ArrowArray) {
    if array.is_null() {
        return;
    }
    let array = &mut *array;

    // take ownership of `private_data`, therefore dropping it
    let private = Box::from_raw(array.private_data as *mut PrivateData);
    for child in private.children_ptr.iter() {
        let _ = Box::from_raw(*child);
    }

    if let Some(ptr) = private.dictionary_ptr {
        let _ = Box::from_raw(ptr);
    }

    array.release = None;
}

#[allow(dead_code)]
struct PrivateData {
    array: Box<dyn Array>,
    buffers_ptr: Box<[*const std::os::raw::c_void]>,
    children_ptr: Box<[*mut ArrowArray]>,
    dictionary_ptr: Option<*mut ArrowArray>,
}

impl ArrowArray {
    /// creates a new `ArrowArray` from existing data.
    /// # Safety
    /// This method releases `buffers`. Consumers of this struct *must* call `release` before
    /// releasing this struct, or contents in `buffers` leak.
    pub(crate) fn new(array: Box<dyn Array>) -> Self {
        let (offset, buffers, children, dictionary) =
            offset_buffers_children_dictionary(array.as_ref());

        let buffers_ptr = buffers
            .iter()
            .map(|maybe_buffer| match maybe_buffer {
                Some(b) => b.as_ptr() as *const std::os::raw::c_void,
                None => std::ptr::null(),
            })
            .collect::<Box<[_]>>();
        let n_buffers = buffers.len() as i64;

        let children_ptr = children
            .into_iter()
            .map(|child| Box::into_raw(Box::new(ArrowArray::new(child))))
            .collect::<Box<_>>();
        let n_children = children_ptr.len() as i64;

        let dictionary_ptr =
            dictionary.map(|array| Box::into_raw(Box::new(ArrowArray::new(array))));

        let length = array.len() as i64;
        let null_count = array.null_count() as i64;

        let mut private_data = Box::new(PrivateData {
            array,
            buffers_ptr,
            children_ptr,
            dictionary_ptr,
        });

        Self {
            length,
            null_count,
            offset: offset as i64,
            n_buffers,
            n_children,
            buffers: private_data.buffers_ptr.as_mut_ptr(),
            children: private_data.children_ptr.as_mut_ptr(),
            dictionary: private_data.dictionary_ptr.unwrap_or(std::ptr::null_mut()),
            release: Some(c_release_array),
            private_data: Box::into_raw(private_data) as *mut ::std::os::raw::c_void,
        }
    }

    /// creates an empty [`ArrowArray`], which can be used to import data into
    pub fn empty() -> Self {
        Self {
            length: 0,
            null_count: 0,
            offset: 0,
            n_buffers: 0,
            n_children: 0,
            buffers: std::ptr::null_mut(),
            children: std::ptr::null_mut(),
            dictionary: std::ptr::null_mut(),
            release: None,
            private_data: std::ptr::null_mut(),
        }
    }

    /// the length of the array
    pub(crate) fn len(&self) -> usize {
        self.length as usize
    }

    /// the offset of the array
    pub(crate) fn offset(&self) -> usize {
        self.offset as usize
    }

    /// the null count of the array
    pub(crate) fn null_count(&self) -> usize {
        self.null_count as usize
    }
}

/// interprets the buffer `index` as a [`Buffer`].
/// # Safety
/// The caller must guarantee that the buffer `index` corresponds to a buffer of type `T`.
/// This function assumes that the buffer created from FFI is valid; this is impossible to prove.
unsafe fn create_buffer<T: NativeType>(
    array: &ArrowArray,
    data_type: &DataType,
    owner: Box<InternalArrowArray>,
    index: usize,
) -> Result<Buffer<T>> {
    if array.buffers.is_null() {
        return Err(Error::OutOfSpec("The array buffers are null".to_string()));
    }

    let buffers = array.buffers as *mut *const u8;

    assert!(index < array.n_buffers as usize);
    let ptr = *buffers.add(index);
    let ptr = NonNull::new(ptr as *mut T);

    let len = buffer_len(array, data_type, index)?;
    let offset = buffer_offset(array, data_type, index);
    let bytes = ptr
        .map(|ptr| Bytes::from_owned(ptr, len, owner))
        .ok_or_else(|| Error::OutOfSpec(format!("The buffer at position {} is null", index)))?;

    Ok(Buffer::from_bytes(bytes).slice(offset, len - offset))
}

/// returns a new buffer corresponding to the index `i` of the FFI array. It may not exist (null pointer).
/// `bits` is the number of bits that the native type of this buffer has.
/// The size of the buffer will be `ceil(self.length * bits, 8)`.
/// # Panic
/// This function panics if `i` is larger or equal to `n_buffers`.
/// # Safety
/// This function assumes that `ceil(self.length * bits, 8)` is the size of the buffer
unsafe fn create_bitmap(
    array: &ArrowArray,
    owner: Box<InternalArrowArray>,
    index: usize,
) -> Result<Bitmap> {
    if array.buffers.is_null() {
        return Err(Error::OutOfSpec("The array buffers are null".to_string()));
    }
    let len = array.length as usize;
    let offset = array.offset as usize;
    let buffers = array.buffers as *mut *const u8;

    assert!(index < array.n_buffers as usize);
    let ptr = *buffers.add(index);

    let bytes_len = bytes_for(offset + len);
    let ptr = NonNull::new(ptr as *mut u8);
    let bytes = ptr
        .map(|ptr| Bytes::from_owned(ptr, bytes_len, owner))
        .ok_or_else(|| {
            Error::OutOfSpec(format!(
                "The buffer {} is a null pointer and cannot be interpreted as a bitmap",
                index
            ))
        })?;

    Ok(Bitmap::from_bytes(bytes, offset + len).slice(offset, len))
}

fn buffer_offset(array: &ArrowArray, data_type: &DataType, i: usize) -> usize {
    use PhysicalType::*;
    match (data_type.to_physical_type(), i) {
        (LargeUtf8, 2) | (LargeBinary, 2) | (Utf8, 2) | (Binary, 2) => 0,
        (FixedSizeBinary, 1) => {
            if let DataType::FixedSizeBinary(size) = data_type.to_logical_type() {
                (array.offset as usize) * *size
            } else {
                unreachable!()
            }
        }
        _ => array.offset as usize,
    }
}

/// Returns the length, in slots, of the buffer `i` (indexed according to the C data interface)
// Rust implementation uses fixed-sized buffers, which require knowledge of their `len`.
// for variable-sized buffers, such as the second buffer of a stringArray, we need
// to fetch offset buffer's len to build the second buffer.
fn buffer_len(array: &ArrowArray, data_type: &DataType, i: usize) -> Result<usize> {
    Ok(match (data_type.to_physical_type(), i) {
        (PhysicalType::FixedSizeBinary, 1) => {
            if let DataType::FixedSizeBinary(size) = data_type.to_logical_type() {
                *size * (array.offset as usize + array.length as usize)
            } else {
                unreachable!()
            }
        }
        (PhysicalType::FixedSizeList, 1) => {
            if let DataType::FixedSizeList(_, size) = data_type.to_logical_type() {
                *size * (array.offset as usize + array.length as usize)
            } else {
                unreachable!()
            }
        }
        (PhysicalType::Utf8, 1)
        | (PhysicalType::LargeUtf8, 1)
        | (PhysicalType::Binary, 1)
        | (PhysicalType::LargeBinary, 1)
        | (PhysicalType::List, 1)
        | (PhysicalType::LargeList, 1)
        | (PhysicalType::Map, 1) => {
            // the len of the offset buffer (buffer 1) equals length + 1
            array.offset as usize + array.length as usize + 1
        }
        (PhysicalType::Utf8, 2) | (PhysicalType::Binary, 2) => {
            // the len of the data buffer (buffer 2) equals the last value of the offset buffer (buffer 1)
            let len = buffer_len(array, data_type, 1)?;
            // first buffer is the null buffer => add(1)
            let offset_buffer = unsafe { *(array.buffers as *mut *const u8).add(1) };
            // interpret as i32
            let offset_buffer = offset_buffer as *const i32;
            // get last offset

            (unsafe { *offset_buffer.add(len - 1) }) as usize
        }
        (PhysicalType::LargeUtf8, 2) | (PhysicalType::LargeBinary, 2) => {
            // the len of the data buffer (buffer 2) equals the last value of the offset buffer (buffer 1)
            let len = buffer_len(array, data_type, 1)?;
            // first buffer is the null buffer => add(1)
            let offset_buffer = unsafe { *(array.buffers as *mut *const u8).add(1) };
            // interpret as i64
            let offset_buffer = offset_buffer as *const i64;
            // get last offset
            (unsafe { *offset_buffer.add(len - 1) }) as usize
        }
        // buffer len of primitive types
        _ => array.offset as usize + array.length as usize,
    })
}

fn create_child(
    array: &ArrowArray,
    field: &DataType,
    parent: Box<InternalArrowArray>,
    index: usize,
) -> Result<ArrowArrayChild<'static>> {
    let data_type = get_child(field, index)?;
    assert!(index < array.n_children as usize);
    assert!(!array.children.is_null());
    unsafe {
        let arr_ptr = *array.children.add(index);
        assert!(!arr_ptr.is_null());
        let arr_ptr = &*arr_ptr;

        Ok(ArrowArrayChild::from_raw(arr_ptr, data_type, parent))
    }
}

fn create_dictionary(
    array: &ArrowArray,
    data_type: &DataType,
    parent: Box<InternalArrowArray>,
) -> Result<Option<ArrowArrayChild<'static>>> {
    if let DataType::Dictionary(_, values, _) = data_type {
        let data_type = values.as_ref().clone();
        assert!(!array.dictionary.is_null());
        let array = unsafe { &*array.dictionary };
        Ok(Some(ArrowArrayChild::from_raw(array, data_type, parent)))
    } else {
        Ok(None)
    }
}

pub trait ArrowArrayRef: std::fmt::Debug {
    fn owner(&self) -> Box<InternalArrowArray> {
        (*self.parent()).clone()
    }

    /// returns the null bit buffer.
    /// Rust implementation uses a buffer that is not part of the array of buffers.
    /// The C Data interface's null buffer is part of the array of buffers.
    /// # Safety
    /// The caller must guarantee that the buffer `index` corresponds to a bitmap.
    /// This function assumes that the bitmap created from FFI is valid; this is impossible to prove.
    unsafe fn validity(&self) -> Result<Option<Bitmap>> {
        if self.array().null_count() == 0 {
            Ok(None)
        } else {
            create_bitmap(self.array(), self.owner(), 0).map(Some)
        }
    }

    /// # Safety
    /// The caller must guarantee that the buffer `index` corresponds to a bitmap.
    /// This function assumes that the bitmap created from FFI is valid; this is impossible to prove.
    unsafe fn buffer<T: NativeType>(&self, index: usize) -> Result<Buffer<T>> {
        create_buffer::<T>(self.array(), self.data_type(), self.owner(), index)
    }

    /// # Safety
    /// The caller must guarantee that the buffer `index` corresponds to a bitmap.
    /// This function assumes that the bitmap created from FFI is valid; this is impossible to prove.
    unsafe fn bitmap(&self, index: usize) -> Result<Bitmap> {
        create_bitmap(self.array(), self.owner(), index)
    }

    /// # Safety
    /// The caller must guarantee that the child `index` is valid per c data interface.
    unsafe fn child(&self, index: usize) -> Result<ArrowArrayChild> {
        create_child(self.array(), self.data_type(), self.parent().clone(), index)
    }

    fn dictionary(&self) -> Result<Option<ArrowArrayChild>> {
        create_dictionary(self.array(), self.data_type(), self.parent().clone())
    }

    fn n_buffers(&self) -> usize;

    fn parent(&self) -> &Box<InternalArrowArray>;
    fn array(&self) -> &ArrowArray;
    fn data_type(&self) -> &DataType;
}

/// Struct used to move an Array from and to the C Data Interface.
/// Its main responsibility is to expose functionality that requires
/// both [ArrowArray] and [ArrowSchema].
///
/// This struct has two main paths:
///
/// ## Import from the C Data Interface
/// * [InternalArrowArray::empty] to allocate memory to be filled by an external call
/// * [InternalArrowArray::try_from_raw] to consume two non-null allocated pointers
/// ## Export to the C Data Interface
/// * [InternalArrowArray::try_new] to create a new [InternalArrowArray] from Rust-specific information
/// * [InternalArrowArray::into_raw] to expose two pointers for [ArrowArray] and [ArrowSchema].
///
/// # Safety
/// Whoever creates this struct is responsible for releasing their resources. Specifically,
/// consumers *must* call [InternalArrowArray::into_raw] and take ownership of the individual pointers,
/// calling [ArrowArray::release] and [ArrowSchema::release] accordingly.
///
/// Furthermore, this struct assumes that the incoming data agrees with the C data interface.
#[derive(Debug, Clone)]
pub struct InternalArrowArray {
    // Arc is used for sharability since this is immutable
    array: Arc<ArrowArray>,
    // Arced to reduce cost of cloning
    data_type: Arc<DataType>,
}

impl InternalArrowArray {
    pub fn new(array: ArrowArray, data_type: DataType) -> Self {
        Self {
            array: Arc::new(array),
            data_type: Arc::new(data_type),
        }
    }
}

impl ArrowArrayRef for Box<InternalArrowArray> {
    /// the data_type as declared in the schema
    fn data_type(&self) -> &DataType {
        &self.data_type
    }

    fn parent(&self) -> &Box<InternalArrowArray> {
        self
    }

    fn array(&self) -> &ArrowArray {
        self.array.as_ref()
    }

    fn n_buffers(&self) -> usize {
        self.array.n_buffers as usize
    }
}

#[derive(Debug)]
pub struct ArrowArrayChild<'a> {
    array: &'a ArrowArray,
    data_type: DataType,
    parent: Box<InternalArrowArray>,
}

impl<'a> ArrowArrayRef for ArrowArrayChild<'a> {
    /// the data_type as declared in the schema
    fn data_type(&self) -> &DataType {
        &self.data_type
    }

    fn parent(&self) -> &Box<InternalArrowArray> {
        &self.parent
    }

    fn array(&self) -> &ArrowArray {
        self.array
    }

    fn n_buffers(&self) -> usize {
        self.array.n_buffers as usize
    }
}

impl<'a> ArrowArrayChild<'a> {
    fn from_raw(
        array: &'a ArrowArray,
        data_type: DataType,
        parent: Box<InternalArrowArray>,
    ) -> Self {
        Self {
            array,
            data_type,
            parent,
        }
    }
}
