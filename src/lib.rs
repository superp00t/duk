//! A high-level wrapper around the [Duktape][1] Javascript/EcmaScript
//! interpreter.
//!
//! Currently, the focus is around supporting "extension"/"plug-in"
//! use cases, so the primary supported functionality is:
//!
//!   * Loading code.
//!   * Calling functions and getting their result.
//!
//! Other use-cases (like exposing Rust functions to JS) are not yet
//! implemented.
//!
//! [1]: http://duktape.org/

extern crate duktape_sys;
#[cfg(feature = "logging")]
#[macro_use]
extern crate log;

use std::collections;
use std::error;
use std::ffi;
use std::fmt;
use std::mem;
use std::os;
use std::path;
use std::ptr;
use std::result;
use std::slice;
use std::str;
use std::sync::atomic;

/// A context corresponding to a thread of script execution.
#[derive(Debug)]
pub struct Context {
    raw: *mut duktape_sys::duk_context,
    next_stash_idx: atomic::AtomicUsize,
}

/// Something that can be used as an argument when calling into Javascript code.
pub trait Argument {
    /// Pushes this argument to the stack of the specified context.  This requires interaction with
    /// the internals of the context, and is therefore an unsafe operation.
    unsafe fn push_to_context(&self, context: &Context);
}

/// A reference to a value that lives within a `Context`.
#[derive(Debug)]
pub struct Reference<'a> {
    ctx: &'a Context,
    stash_idx: duktape_sys::duk_uarridx_t,
}

/// A Javascript/Ecmascript value that exists in the Rust world.
///
/// Duktape supports values beyond these, but they don't have good Rust semantics, so they cannot be
/// interacted with from the Rust world.  They are therefore mapped to `Value::Foreign` when
/// retrieved, and trying to further use those values is generally equivalent to using `undefined`.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// The `undefined` value.
    Undefined,
    /// The `null` value.
    Null,
    /// A boolean like `true` or `false`.
    Boolean(bool),
    /// Any number (both integral like `5` and fractional like `2.3`).
    Number(f64),
    /// Any string like `'abc'`.
    String(String),
    /// Any array of values like `['a', 2, false]`.
    Array(Vec<Value>),
    /// A JSON-like object like `{a: 'a', b: 2, c: false}`.
    Object(collections::BTreeMap<String, Value>),
    /// A Duktape byte buffer like `Duktape.Buffer('abc')`.
    Bytes(Vec<u8>),
    /// A Duktape value that cannot be represented in Rust (yet).
    ///
    /// Contains a `&str` describing the foreign type.
    Foreign(&'static str),
}

/// The type of errors that might occur.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Error {
    /// An error that originates from executing Javascript/Ecmascript.
    Js {
        /// The kind of error.
        kind: JsErrorKind,
        /// A descriptive user-controlled error message.
        message: String,
        file_name: Option<String>,
        line_number: Option<usize>,
        stack: Option<String>,
    },
}

/// Kinds of Javascript/Ecmascript errors
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JsErrorKind {
    /// A thrown error that doesn't inherit from `Error`, like when
    /// the user does `throw 3.14;`.
    Generic,

    /// An error that's an instance of `Error`.
    Error,
    /// An error that's an instance of `EvalError`.
    Eval,
    /// An error that's an instance of `RangeError`.
    Range,
    /// An error that's an instance of `ReferenceError`.
    Reference,
    /// An error that's an instance of `SyntaxError`.
    Syntax,
    /// An error that's an instance of `TypeError`.
    Type,
    /// An error that's an instance of `UriError`.
    Uri,
}

/// Convenience type for results using the `Error` type.
pub type Result<A> = result::Result<A, Error>;

impl Context {
    /// Creates a new context.
    pub fn new() -> Context {
        let raw = unsafe {
            duktape_sys::duk_create_heap(None, None, None, ptr::null_mut(), Some(fatal_handler))
        };

        unsafe {
            Context::setup_logging(raw);
        }

        Context {
            raw: raw,
            next_stash_idx: atomic::ATOMIC_USIZE_INIT,
        }
    }

    #[cfg(feature = "logging")]
    unsafe fn setup_logging(ctx: *mut duktape_sys::duk_context) {
        use duktape_sys::*;
        duk_logging_init(ctx, 0);

        duk_push_global_object(ctx);
        duk_get_prop_string(ctx, -1, nul_str(b"Duktape\0"));
        duk_get_prop_string(ctx, -1, nul_str(b"Logger\0"));
        duk_get_prop_string(ctx, -1, nul_str(b"prototype\0"));
        // Stack: [ global .Duktape .Logger .prototype ]

        duk_push_c_function(ctx, Some(log_handler), DUK_VARARGS);
        duk_set_magic(ctx, -1, DUK_LOG_TRACE);
        duk_put_prop_string(ctx, -2, nul_str(b"trace\0"));

        duk_push_c_function(ctx, Some(log_handler), DUK_VARARGS);
        duk_set_magic(ctx, -1, DUK_LOG_DEBUG);
        duk_put_prop_string(ctx, -2, nul_str(b"debug\0"));

        duk_push_c_function(ctx, Some(log_handler), DUK_VARARGS);
        duk_set_magic(ctx, -1, DUK_LOG_INFO);
        duk_put_prop_string(ctx, -2, nul_str(b"info\0"));

        duk_push_c_function(ctx, Some(log_handler), DUK_VARARGS);
        duk_set_magic(ctx, -1, DUK_LOG_WARN);
        duk_put_prop_string(ctx, -2, nul_str(b"warn\0"));

        duk_push_c_function(ctx, Some(log_handler), DUK_VARARGS);
        duk_set_magic(ctx, -1, DUK_LOG_ERROR);
        duk_put_prop_string(ctx, -2, nul_str(b"error\0"));

        duk_push_c_function(ctx, Some(log_handler), DUK_VARARGS);
        duk_set_magic(ctx, -1, DUK_LOG_FATAL);
        duk_put_prop_string(ctx, -2, nul_str(b"fatal\0"));

        // Stack: [ global .Duktape .Logger .prototype ]
        duk_pop_n(ctx, 4);
    }

    #[cfg(not(feature = "logging"))]
    unsafe fn setup_logging(_: *mut duktape_sys::duk_context) {
        // No-op
    }

    /// Evaluates the specified script string within the current
    /// context.
    ///
    /// # Examples
    ///
    /// Successful evaluation:
    ///
    /// ```
    /// let mut ctx = duk::Context::new();
    /// let value = ctx.eval_string("'ab' + 'cd' + Math.floor(2.3)").unwrap().to_value();
    /// assert_eq!(duk::Value::String("abcd2".to_owned()), value);
    /// ```
    ///
    /// However, if we try to call a function that doesn't exist:
    ///
    /// ```
    /// let ctx = duk::Context::new();
    /// let result = ctx.eval_string("var a = {}; a.foo()");
    /// match result {
    ///   Err(duk::Error::Js { kind, message, .. }) => {
    ///     assert_eq!(duk::JsErrorKind::Type, kind);
    ///     assert_eq!("undefined not callable", message);
    ///   },
    ///   _ => unreachable!(),
    /// }
    /// ```
    pub fn eval_string(&self, string: &str) -> Result<Reference> {
        let ptr = string.as_ptr() as *const i8;
        let len = string.len();
        unsafe {
            let ret = duktape_sys::duk_peval_lstring(self.raw, ptr, len);
            self.pop_reference_or_error(ret)
        }
    }

    /// Like `eval_string`, but sets the file name for all of the evaluated functions to the
    /// specified string.
    pub fn eval_string_with_filename(&self, filename: &str, string: &str) -> Result<Reference> {
        let filename_ptr = filename.as_ptr() as *const i8;
        let string_ptr = string.as_ptr() as *const i8;
        unsafe {
            duktape_sys::duk_push_lstring(self.raw, filename_ptr, filename.len());
            let flags = duktape_sys::DUK_COMPILE_EVAL | duktape_sys::DUK_COMPILE_NOSOURCE |
                        duktape_sys::DUK_COMPILE_SAFE;
            let ret = duktape_sys::duk_eval_raw(self.raw, string_ptr, string.len(), flags);
            self.pop_reference_or_error(ret)
        }
    }

    /// Loads and evaluates the specified file within the current
    /// context.
    pub fn eval_file(&self, path: &path::Path) -> Result<Reference> {
        let str_path = path.to_string_lossy();
        let ffi_str = ffi::CString::new(&*str_path).unwrap();
        unsafe {
            let ret = duktape_sys::duk_peval_file(self.raw, ffi_str.as_ptr());
            self.pop_reference_or_error(ret)
        }
    }

    /// Retrieves a reference to the global object.
    pub fn global_object(&self) -> Reference {
        unsafe {
            duktape_sys::duk_push_global_object(self.raw);
            self.pop_reference()
        }
    }

    /// Calls the specified global script function with the supplied
    /// arguments.
    ///
    /// Behaves like `global_object().call_method(name, args)`.
    pub fn call_global(&self, name: &str, args: &[&Argument]) -> Result<Reference> {
        self.global_object().call_method(name, args)
    }

    #[cfg(test)]
    pub fn assert_clean(&self) {
        unsafe {
            assert!(duktape_sys::duk_get_top(self.raw) == 0,
                    "context stack is not empty");
        }
    }

    fn gen_stash_idx(&self) -> duktape_sys::duk_uarridx_t {
        self.next_stash_idx.fetch_add(1, atomic::Ordering::Relaxed) as duktape_sys::duk_uarridx_t
    }

    unsafe fn pop_reference(&self) -> Reference {
        let idx = self.gen_stash_idx();
        duktape_sys::duk_push_heap_stash(self.raw);
        duktape_sys::duk_dup(self.raw, -2);
        duktape_sys::duk_put_prop_index(self.raw, -2, idx);
        duktape_sys::duk_pop_2(self.raw);

        Reference {
            ctx: self,
            stash_idx: idx,
        }
    }

    unsafe fn pop_error(&self) -> Error {
        let e = Error::get(self.raw, -1);
        duktape_sys::duk_pop(self.raw);
        e
    }

    unsafe fn pop_reference_or_error(&self, ret: duktape_sys::duk_ret_t) -> Result<Reference> {
        if ret == 0 {
            Ok(self.pop_reference())
        } else {
            Err(self.pop_error())
        }
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        unsafe { duktape_sys::duk_destroy_heap(self.raw) };
    }
}

impl<'a> Reference<'a> {
    /// Converts this reference to a `Value` which can be used for further processing by Rust code.
    pub fn to_value(&self) -> Value {
        self.with_value(|| { unsafe { Value::get(self.ctx.raw, -1) } })
    }

    /// Gets the property with the specified key, provided that this reference points to something
    /// that is object coercible.
    pub fn get(&self, name: &str) -> Result<Reference<'a>> {
        let ffi_str = ffi::CString::new(name).unwrap();
        self.with_value(|| {
            unsafe {
                if 0 == duktape_sys::duk_is_object_coercible(self.ctx.raw, -1) {
                    let msg = ffi::CString::new("value is not object coercible").unwrap();
                    duktape_sys::duk_push_error_object(self.ctx.raw,
                                                       duktape_sys::DUK_ERR_TYPE_ERROR,
                                                       msg.as_ptr());
                    Err(self.ctx.pop_error())
                } else {
                    duktape_sys::duk_get_prop_string(self.ctx.raw, -1, ffi_str.as_ptr());
                    Ok(self.ctx.pop_reference())
                }
            }
        })
    }

    /// Calls the function that this reference points to without a `this` binding, using the
    /// specified arguments.
    ///
    /// When the function executes, the `this` binding is set to `undefined` or the global object,
    /// depending on if the function is strict or not.  Calling this function is equivalent to doing
    /// `myfunc.call(undefined, args)` in Javascript.
    pub fn call(&self, args: &[&Argument]) -> Result<Reference<'a>> {
        self.with_value(|| {
            unsafe {
                duktape_sys::duk_dup_top(self.ctx.raw); // Because pcall consumes the stack
                for arg in args {
                    arg.push_to_context(self.ctx);
                }
                let ret = duktape_sys::duk_pcall(self.ctx.raw,
                                                 args.len() as duktape_sys::duk_idx_t);
                self.ctx.pop_reference_or_error(ret)
            }
        })
    }

    /// Calls the function that this reference points to with an explicit `this` binding.
    pub fn call_with_this(&self, this: &Argument, args: &[&Argument]) -> Result<Reference<'a>> {
        self.with_value(|| {
            unsafe {
                duktape_sys::duk_dup_top(self.ctx.raw); // Because pcall consumes the stack
                this.push_to_context(self.ctx);

                for arg in args {
                    arg.push_to_context(self.ctx);
                }
                let ret = duktape_sys::duk_pcall_method(self.ctx.raw,
                                                        args.len() as duktape_sys::duk_idx_t);
                self.ctx.pop_reference_or_error(ret)
            }
        })
    }

    /// Calls a method on the object that this reference points to.
    ///
    /// The `this` binding will be set to the object during the execution of the function.  Calling
    /// this function is equivalent to doing `myobj[name](args...)` in Javascript.
    pub fn call_method(&self, name: &str, args: &[&Argument]) -> Result<Reference<'a>> {
        self.with_value(|| {
            unsafe {
                let obj_idx = duktape_sys::duk_get_top_index(self.ctx.raw);
                duktape_sys::duk_push_lstring(self.ctx.raw, name.as_ptr() as *const i8, name.len());

                for arg in args {
                    arg.push_to_context(self.ctx);
                }

                let ret = duktape_sys::duk_pcall_prop(self.ctx.raw,
                                                      obj_idx,
                                                      args.len() as duktape_sys::duk_idx_t);

                self.ctx.pop_reference_or_error(ret)
            }
        })
    }

    /// Calls the function that this reference points to as a constructor, with the specified
    /// arguments.
    pub fn new(&self, args: &[&Argument]) -> Result<Reference<'a>> {
        self.with_value(|| {
            unsafe {
                duktape_sys::duk_dup_top(self.ctx.raw); // Because pnew consumes the stack
                for arg in args {
                    arg.push_to_context(self.ctx);
                }
                let ret = duktape_sys::duk_pnew(self.ctx.raw, args.len() as duktape_sys::duk_idx_t);
                self.ctx.pop_reference_or_error(ret)
            }
        })
    }

    #[inline]
    fn with_value<F, R>(&self, action: F) -> R
        where F: FnOnce() -> R
    {
        unsafe {
            self.push();
            let result = action();
            self.pop();
            result
        }
    }

    unsafe fn push(&self) {
        duktape_sys::duk_push_heap_stash(self.ctx.raw);
        duktape_sys::duk_get_prop_index(self.ctx.raw, -1, self.stash_idx);
        duktape_sys::duk_remove(self.ctx.raw, -2);
    }

    unsafe fn pop(&self) {
        duktape_sys::duk_pop(self.ctx.raw);
    }
}

impl<'a> Argument for Reference<'a> {
    unsafe fn push_to_context(&self, context: &Context) {
        if context.raw != self.ctx.raw {
            panic!("Tried to mix references coming from different contexts");
        }

        self.push();
    }
}

impl<'a> PartialEq for Reference<'a> {
    fn eq(&self, _: &Self) -> bool {
        false
    }
}

impl<'a> Drop for Reference<'a> {
    fn drop(&mut self) {
        unsafe {
            duktape_sys::duk_push_heap_stash(self.ctx.raw);
            duktape_sys::duk_del_prop_index(self.ctx.raw, -1, self.stash_idx);
            duktape_sys::duk_pop(self.ctx.raw);
        }
    }
}

impl Value {
    /// Copies this value into a `Context`, and returns the reference to the value within the
    /// context.
    pub fn to_reference<'a>(&self, context: &'a Context) -> Reference<'a> {
        unsafe {
            self.push(context.raw);
            context.pop_reference()
        }
    }

    unsafe fn get(ctx: *mut duktape_sys::duk_context, index: duktape_sys::duk_idx_t) -> Value {
        let t = duktape_sys::duk_get_type(ctx, index);
        if t == duktape_sys::DUK_TYPE_UNDEFINED {
            Value::Undefined
        } else if t == duktape_sys::DUK_TYPE_NULL {
            Value::Null
        } else if t == duktape_sys::DUK_TYPE_BOOLEAN {
            Value::Boolean(duktape_sys::duk_get_boolean(ctx, index) != 0)
        } else if t == duktape_sys::DUK_TYPE_NUMBER {
            Value::Number(duktape_sys::duk_get_number(ctx, index))
        } else if t == duktape_sys::DUK_TYPE_STRING {
            Value::String(get_string(ctx, index))
        } else if t == duktape_sys::DUK_TYPE_OBJECT {
            if 1 == duktape_sys::duk_is_array(ctx, index) {
                let len = duktape_sys::duk_get_length(ctx, index);
                let mut array = Vec::with_capacity(len);

                for i in 0..len {
                    assert!(1 == duktape_sys::duk_get_prop_index(ctx, index, i as u32));
                    array.push(Value::get(ctx, -1));
                    duktape_sys::duk_pop(ctx);
                }

                Value::Array(array)
            } else {
                let mut object = collections::BTreeMap::new();
                duktape_sys::duk_enum(ctx, -1, duktape_sys::DUK_ENUM_OWN_PROPERTIES_ONLY);

                while 1 == duktape_sys::duk_next(ctx, -1, 1) {
                    let key = get_string(ctx, -2);
                    let value = Value::get(ctx, -1);
                    duktape_sys::duk_pop_2(ctx);
                    object.insert(key, value);
                }

                duktape_sys::duk_pop(ctx);

                Value::Object(object)
            }
        } else if t == duktape_sys::DUK_TYPE_BUFFER {
            let mut size = mem::uninitialized();
            let data = duktape_sys::duk_get_buffer(ctx, index, &mut size);
            let slice = slice::from_raw_parts(data as *const u8, size);
            Value::Bytes(slice.to_vec())
        } else if t == duktape_sys::DUK_TYPE_POINTER {
            Value::Foreign("pointer")
        } else if t == duktape_sys::DUK_TYPE_LIGHTFUNC {
            Value::Foreign("lightfunc")
        } else {
            panic!("Unmapped type {}", t)
        }
    }

    unsafe fn push(&self, ctx: *mut duktape_sys::duk_context) {
        match *self {
            Value::Undefined => duktape_sys::duk_push_undefined(ctx),
            Value::Null => duktape_sys::duk_push_null(ctx),
            Value::Boolean(b) => {
                let v = if b {
                    1
                } else {
                    0
                };
                duktape_sys::duk_push_boolean(ctx, v);
            }
            Value::Number(n) => duktape_sys::duk_push_number(ctx, n),
            Value::String(ref string) => {
                let data = string.as_ptr() as *const i8;
                let len = string.len();
                duktape_sys::duk_push_lstring(ctx, data, len);
            }
            Value::Array(ref array) => {
                duktape_sys::duk_push_array(ctx);
                for (i, elem) in array.iter().enumerate() {
                    elem.push(ctx);
                    assert!(1 == duktape_sys::duk_put_prop_index(ctx, -2, i as u32));
                }
            }
            Value::Object(ref object) => {
                duktape_sys::duk_push_object(ctx);

                for (k, v) in object {
                    let k_data = k.as_ptr() as *const i8;
                    let k_len = k.len();
                    duktape_sys::duk_push_lstring(ctx, k_data, k_len);
                    v.push(ctx);
                    duktape_sys::duk_put_prop(ctx, -3);
                }
            }
            Value::Bytes(ref bytes) => {
                let len = bytes.len();
                let data = duktape_sys::duk_push_fixed_buffer(ctx, len);

                ptr::copy(bytes.as_ptr(), data as *mut u8, len);
            }
            Value::Foreign(_) => duktape_sys::duk_push_undefined(ctx),
        }
    }
}

impl Argument for Value {
    unsafe fn push_to_context(&self, context: &Context) {
        self.push(context.raw);
    }
}

impl Error {
    unsafe fn get(ctx: *mut duktape_sys::duk_context, index: duktape_sys::duk_idx_t) -> Error {
        let e = duktape_sys::duk_get_error_code(ctx, index);
        let kind = JsErrorKind::from_raw(e);
        let message = get_string_property(ctx, index, "message").unwrap_or_else(|| {
            let mut len = mem::uninitialized();
            let data = duktape_sys::duk_safe_to_lstring(ctx, index, &mut len);
            let msg_slice = slice::from_raw_parts(data as *const u8, len);
            String::from(str::from_utf8(msg_slice).unwrap())
        });
        let file_name = get_string_property(ctx, index, "fileName").and_then(|n| if n.is_empty() {
            None
        } else {
            Some(n)
        });
        let line_number = get_number_property(ctx, index, "lineNumber")
            .and_then(|n| if n.is_nan() {
                None
            } else {
                Some(n as usize)
            });
        let stack = get_string_property(ctx, index, "stack");

        Error::Js {
            kind: kind,
            message: message,
            file_name: file_name,
            line_number: line_number,
            stack: stack,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Error::Js { ref message, .. } => write!(f, "JS error: {}", message),
        }
    }
}

impl error::Error for Error {
    fn description(&self) -> &str {
        match *self {
            Error::Js { .. } => "JS error",
        }
    }
}

impl JsErrorKind {
    unsafe fn from_raw(e: duktape_sys::duk_errcode_t) -> JsErrorKind {
        if e == duktape_sys::DUK_ERR_NONE {
            JsErrorKind::Generic
        } else if e == duktape_sys::DUK_ERR_ERROR {
            JsErrorKind::Error
        } else if e == duktape_sys::DUK_ERR_EVAL_ERROR {
            JsErrorKind::Eval
        } else if e == duktape_sys::DUK_ERR_RANGE_ERROR {
            JsErrorKind::Range
        } else if e == duktape_sys::DUK_ERR_REFERENCE_ERROR {
            JsErrorKind::Reference
        } else if e == duktape_sys::DUK_ERR_SYNTAX_ERROR {
            JsErrorKind::Syntax
        } else if e == duktape_sys::DUK_ERR_TYPE_ERROR {
            JsErrorKind::Type
        } else if e == duktape_sys::DUK_ERR_URI_ERROR {
            JsErrorKind::Uri
        } else {
            panic!("Unmapped error code {}", e)
        }
    }
}

unsafe fn get_string(ctx: *mut duktape_sys::duk_context, index: duktape_sys::duk_idx_t) -> String {
    let mut len = mem::uninitialized();
    let data = duktape_sys::duk_get_lstring(ctx, index, &mut len);
    let slice = slice::from_raw_parts(data as *const u8, len);
    String::from(str::from_utf8(slice).unwrap())
}

unsafe fn get_string_property(ctx: *mut duktape_sys::duk_context,
                              index: duktape_sys::duk_idx_t,
                              name: &str)
                              -> Option<String> {
    let ffi_name = ffi::CString::new(name).unwrap();
    if 1 == duktape_sys::duk_get_prop_string(ctx, index, ffi_name.as_ptr()) {
        let result = get_string(ctx, -1);
        duktape_sys::duk_pop(ctx);

        Some((*result).to_owned())
    } else {
        duktape_sys::duk_pop(ctx);
        None
    }
}

unsafe fn get_number_property(ctx: *mut duktape_sys::duk_context,
                              index: duktape_sys::duk_idx_t,
                              name: &str)
                              -> Option<f64> {
    let ffi_name = ffi::CString::new(name).unwrap();
    if 1 == duktape_sys::duk_get_prop_string(ctx, index, ffi_name.as_ptr()) {
        let result = duktape_sys::duk_get_number(ctx, -1);
        duktape_sys::duk_pop(ctx);
        Some(result)
    } else {
        duktape_sys::duk_pop(ctx);
        None
    }
}

unsafe fn nul_str(data: &[u8]) -> *const os::raw::c_char {
    ffi::CStr::from_bytes_with_nul_unchecked(data).as_ptr()
}

#[cfg(feature = "logging")]
unsafe extern "C" fn log_handler(ctx: *mut duktape_sys::duk_context) -> duktape_sys::duk_ret_t {
    use duktape_sys::*; // Because this function is essentially only calling C stuff

    // The function magic is the log level that this handler should handle.
    let level = duk_get_current_magic(ctx);
    if level < DUK_LOG_TRACE || level > DUK_LOG_FATAL {
        warn!("log_handler called with invalid level: {}", level);
        return 0;
    }

    // Stack: [ arg0 ... argN ]
    let nargs = duk_get_top(ctx);

    duk_push_this(ctx);
    // Stack: [ arg0 ... argN this ]

    duk_get_prop_string(ctx, -1, nul_str(b"l\0"));
    // Stack: [ arg0 ... argN this loggerLevel ]

    // Check if we should log this level with this logger
    let logger_level = duk_get_int(ctx, -1);
    if level < logger_level {
        return 0;
    }

    let rust_level = if logger_level == DUK_LOG_TRACE {
        log::LogLevel::Trace
    } else if logger_level == DUK_LOG_DEBUG {
        log::LogLevel::Debug
    } else if logger_level == DUK_LOG_INFO {
        log::LogLevel::Info
    } else if logger_level == DUK_LOG_WARN {
        log::LogLevel::Warn
    } else {
        log::LogLevel::Error
    };

    duk_get_prop_string(ctx, -2, nul_str(b"n\0"));
    // Stack: [ arg0 ... argN this loggerLevel loggerName ]
    duk_to_string(ctx, -1);

    let mut total_len = 0;

    // Replace all args with equivalent strings, and compute their lengths
    // Stack: [ arg0 ... argN this loggerLevel loggerName ]
    for i in 0..nargs {
        if 1 == duk_is_object(ctx, i) {
            duk_push_string(ctx, nul_str(b"fmt\0"));
            duk_dup(ctx, i);
            // Stack: [ arg1 ... argN this loggerLevel loggerName 'fmt' arg ]
            // Call: this.fmt(arg) so -5 is this
            duk_pcall_prop(ctx, -5, 1);
            duk_replace(ctx, i);
        }

        let mut arg_len = mem::uninitialized();

        duk_to_lstring(ctx, i, &mut arg_len);

        total_len += arg_len as usize;
    }

    // Stack: [ arg0String ... argNString this loggerLevel loggerName ]

    let mut name_len = mem::uninitialized();
    let name_data = duk_get_lstring(ctx, -1, &mut name_len);
    let name_slice = slice::from_raw_parts(name_data as *const u8, name_len);
    let name_str = str::from_utf8(name_slice).unwrap();

    // Allocate message space; include nargs to allocate spaces
    let mut msg = String::with_capacity(total_len + name_str.len() + nargs as usize + 1);
    msg.push_str(name_str);
    msg.push(':');

    for i in 0..nargs {
        let mut arg_len = mem::uninitialized();
        let arg_data = duk_get_lstring(ctx, i, &mut arg_len);
        let slice = slice::from_raw_parts(arg_data as *const u8, arg_len);
        let arg_str = str::from_utf8(slice).unwrap();

        msg.push(' ');
        msg.push_str(arg_str);
    }

    log!(target: &format!("{}:{}", module_path!(), name_str), rust_level, "{}",  msg);

    0
}

unsafe extern "C" fn fatal_handler(_: *mut os::raw::c_void, msg_raw: *const os::raw::c_char) {
    let msg = &*ffi::CStr::from_ptr(msg_raw).to_string_lossy();
    // TODO: No unwind support from C... but this "works" right now
    panic!("Duktape fatal error: {}", msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections;

    fn clean_error<A>(result: &mut Result<A>) {
        if let &mut Err(Error::Js { ref mut file_name, ref mut line_number, ref mut stack, .. }) =
               result {
            *file_name = None;
            *line_number = None;
            *stack = None;
        }
    }

    #[test]
    fn eval_string_undefined() {
        let ctx = Context::new();
        let value = ctx.eval_string("undefined").unwrap().to_value();
        assert_eq!(Value::Undefined, value);
        ctx.assert_clean();
    }

    #[test]
    fn eval_string_null() {
        let ctx = Context::new();
        let value = ctx.eval_string("null").unwrap().to_value();
        assert_eq!(Value::Null, value);
        ctx.assert_clean();
    }

    #[test]
    fn eval_string_boolean_true() {
        let ctx = Context::new();
        let value = ctx.eval_string("true").unwrap().to_value();
        assert_eq!(Value::Boolean(true), value);
        ctx.assert_clean();
    }

    #[test]
    fn eval_string_boolean_false() {
        let ctx = Context::new();
        let value = ctx.eval_string("false").unwrap().to_value();
        assert_eq!(Value::Boolean(false), value);
        ctx.assert_clean();
    }

    #[test]
    fn eval_string_number_integral() {
        let ctx = Context::new();
        let value = ctx.eval_string("4").unwrap().to_value();
        assert_eq!(Value::Number(4.0), value);
        ctx.assert_clean();
    }

    #[test]
    fn eval_string_number_fractional() {
        let ctx = Context::new();
        let value = ctx.eval_string("0.5").unwrap().to_value();
        assert_eq!(Value::Number(0.5), value);
        ctx.assert_clean();
    }

    #[test]
    fn eval_string_string() {
        let ctx = Context::new();
        let value = ctx.eval_string("'ab'").unwrap().to_value();
        assert_eq!(Value::String("ab".to_owned()), value);
        ctx.assert_clean();
    }

    #[test]
    fn eval_string_array() {
        let ctx = Context::new();
        let value = ctx.eval_string("['a', 3, false]").unwrap().to_value();
        assert_eq!(Value::Array(vec![Value::String("a".to_owned()),
                                     Value::Number(3.0),
                                     Value::Boolean(false)]),
                   value);
        ctx.assert_clean();
    }

    #[test]
    fn eval_string_object() {
        let ctx = Context::new();
        let value = ctx.eval_string("({a: 'a', b: 3, c: false})").unwrap().to_value();

        let mut expected = collections::BTreeMap::new();
        expected.insert("a".to_owned(), Value::String("a".to_owned()));
        expected.insert("b".to_owned(), Value::Number(3.0));
        expected.insert("c".to_owned(), Value::Boolean(false));

        assert_eq!(Value::Object(expected), value);
        ctx.assert_clean();
    }

    #[test]
    fn eval_string_buffer() {
        let ctx = Context::new();
        let value = ctx.eval_string("Duktape.Buffer('abc')").unwrap().to_value();
        assert_eq!(Value::Bytes("abc".as_bytes().to_vec()), value);
        ctx.assert_clean();
    }

    #[test]
    fn eval_string_error_generic() {
        let ctx = Context::new();
        let mut value = ctx.eval_string("throw 'foobar';");
        clean_error(&mut value);
        assert_eq!(Err(Error::Js {
                       kind: JsErrorKind::Generic,
                       message: "foobar".to_owned(),
                       file_name: None,
                       line_number: None,
                       stack: None,
                   }),
                   value);
        ctx.assert_clean();
    }

    #[test]
    fn eval_string_error_error() {
        let ctx = Context::new();
        let mut value = ctx.eval_string("throw new Error('xyz')");
        clean_error(&mut value);
        assert_eq!(Err(Error::Js {
                       kind: JsErrorKind::Error,
                       message: "xyz".to_owned(),
                       file_name: None,
                       line_number: None,
                       stack: None,
                   }),
                   value);
        ctx.assert_clean();
    }

    #[test]
    fn eval_string_eval_error() {
        let ctx = Context::new();
        let mut value = ctx.eval_string("throw new EvalError('xyz')");
        clean_error(&mut value);
        assert_eq!(Err(Error::Js {
                       kind: JsErrorKind::Eval,
                       message: "xyz".to_owned(),
                       file_name: None,
                       line_number: None,
                       stack: None,
                   }),
                   value);
        ctx.assert_clean();
    }

    #[test]
    fn eval_string_range_error() {
        let ctx = Context::new();
        let mut value = ctx.eval_string("throw new RangeError('xyz')");
        clean_error(&mut value);
        assert_eq!(Err(Error::Js {
                       kind: JsErrorKind::Range,
                       message: "xyz".to_owned(),
                       file_name: None,
                       line_number: None,
                       stack: None,
                   }),
                   value);
        ctx.assert_clean();
    }

    #[test]
    fn eval_string_reference_error() {
        let ctx = Context::new();
        let mut value = ctx.eval_string("throw new ReferenceError('xyz')");
        clean_error(&mut value);
        assert_eq!(Err(Error::Js {
                       kind: JsErrorKind::Reference,
                       message: "xyz".to_owned(),
                       file_name: None,
                       line_number: None,
                       stack: None,
                   }),
                   value);
        ctx.assert_clean();
    }

    #[test]
    fn eval_string_syntax_error() {
        let ctx = Context::new();
        let mut value = ctx.eval_string("throw new SyntaxError('xyz')");
        clean_error(&mut value);
        assert_eq!(Err(Error::Js {
                       kind: JsErrorKind::Syntax,
                       message: "xyz".to_owned(),
                       file_name: None,
                       line_number: None,
                       stack: None,
                   }),
                   value);
        ctx.assert_clean();
    }

    #[test]
    fn eval_string_type_error() {
        let ctx = Context::new();
        let mut value = ctx.eval_string("throw new TypeError('xyz')");
        clean_error(&mut value);
        assert_eq!(Err(Error::Js {
                       kind: JsErrorKind::Type,
                       message: "xyz".to_owned(),
                       file_name: None,
                       line_number: None,
                       stack: None,
                   }),
                   value);
        ctx.assert_clean();
    }

    #[test]
    fn eval_string_uri_error() {
        let ctx = Context::new();
        let mut value = ctx.eval_string("throw new URIError('xyz')");
        clean_error(&mut value);
        assert_eq!(Err(Error::Js {
                       kind: JsErrorKind::Uri,
                       message: "xyz".to_owned(),
                       file_name: None,
                       line_number: None,
                       stack: None,
                   }),
                   value);
        ctx.assert_clean();
    }

    #[test]
    fn eval_string_global_object_get_key_call() {
        let ctx = Context::new();
        ctx.eval_string(r"
          function foo() {
            return 'a';
          }")
            .unwrap();
        let global = ctx.global_object();
        ctx.assert_clean();
        let foo = global.get("foo").unwrap();
        ctx.assert_clean();
        let value = foo.call(&[]).unwrap().to_value();
        assert_eq!(Value::String("a".to_owned()), value);
        ctx.assert_clean();
    }

    #[test]
    fn eval_string_global_object_call_method() {
        let ctx = Context::new();
        ctx.eval_string(r"
          var bar = 2;
          function foo() {
            if (this === undefined || this.bar !== 2) {
              throw 'b';
            }
            return Array.prototype.slice.call(arguments);
          }")
            .unwrap();
        let global = ctx.global_object();
        ctx.assert_clean();
        let value = global.call_method("foo", &[&Value::Number(4.25)]).unwrap().to_value();
        assert_eq!(Value::Array(vec![Value::Number(4.25)]), value);
        ctx.assert_clean();
    }

    #[test]
    fn eval_string_global_object_get_key_call_with_this() {
        let ctx = Context::new();
        ctx.eval_string(r"
          var bar = 2;
          function foo() {
            if (this === undefined || this.bar !== 2) {
              throw 'b';
            }
            return Array.prototype.slice.call(arguments);
          }")
            .unwrap();
        let global = ctx.global_object();
        ctx.assert_clean();
        let foo = global.get("foo").unwrap();
        ctx.assert_clean();
        let value = foo.call_with_this(&global, &[&Value::Number(4.25)]).unwrap().to_value();
        assert_eq!(Value::Array(vec![Value::Number(4.25)]), value);
        ctx.assert_clean();
    }

    #[test]
    fn eval_string_global_object_get_key_new() {
        let ctx = Context::new();
        ctx.eval_string(r"
          function foo() {
            if (this.constructor !== foo) {
              throw 'b';
            }
            return Array.prototype.slice.call(arguments);
          }")
            .unwrap();
        let global = ctx.global_object();
        ctx.assert_clean();
        let foo = global.get("foo").unwrap();
        ctx.assert_clean();
        let value = foo.new(&[&Value::Number(4.25)]).unwrap().to_value();
        assert_eq!(Value::Array(vec![Value::Number(4.25)]), value);
        ctx.assert_clean();
    }

    #[test]
    fn eval_string_call_global() {
        let ctx = Context::new();
        ctx.eval_string(r"
          function foo() {
            return 'a';
          }")
            .unwrap();
        let value = ctx.call_global("foo", &[]).unwrap().to_value();
        assert_eq!(Value::String("a".to_owned()), value);
        ctx.assert_clean();
    }

    #[test]
    fn eval_string_call_global_args() {
        let ctx = Context::new();
        ctx.eval_string(r"
          function foo() {
            return Array.prototype.slice.call(arguments);
          }")
            .unwrap();

        let mut obj = collections::BTreeMap::new();
        obj.insert("a".to_owned(), Value::String("a".to_owned()));
        obj.insert("b".to_owned(), Value::Number(3.0));
        obj.insert("c".to_owned(), Value::Boolean(false));

        let arr = vec![Value::String("a".to_owned()), Value::Number(3.0), Value::Boolean(false)];

        let bytes = vec![0, 1, 2, 3];

        let values = &[Value::Undefined,
                       Value::Null,
                       Value::Boolean(true),
                       Value::Number(1.0),
                       Value::String("foo".to_owned()),
                       Value::Array(arr),
                       Value::Object(obj),
                       Value::Bytes(bytes)];
        let args = values.iter().map(|v| v as &Argument).collect::<Vec<_>>();
        let value = ctx.call_global("foo", &args).unwrap().to_value();
        assert_eq!(Value::Array(values.to_vec()), value);
        ctx.assert_clean();
    }

    #[test]
    fn eval_string_call_global_error() {
        let ctx = Context::new();
        ctx.eval_string(r"
          function foo() {
            throw 'a';
          }")
            .unwrap();
        let mut value = ctx.call_global("foo", &[]);
        clean_error(&mut value);
        assert_eq!(Err(Error::Js {
                       kind: JsErrorKind::Generic,
                       message: "a".to_owned(),
                       file_name: None,
                       line_number: None,
                       stack: None,
                   }),
                   value);
        ctx.assert_clean();
    }

    #[test]
    fn call_non_existent() {
        let ctx = Context::new();
        let mut value = ctx.call_global("foo", &[]);
        clean_error(&mut value);
        assert_eq!(Err(Error::Js {
                       kind: JsErrorKind::Type,
                       message: "undefined not callable".to_owned(),
                       file_name: None,
                       line_number: None,
                       stack: None,
                   }),
                   value);
        ctx.assert_clean();
    }
}
