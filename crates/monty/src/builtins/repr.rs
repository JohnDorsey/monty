//! Implementation of the repr() builtin function.

use crate::native_work;
use crate::{args::ArgValues, bytecode::VM, defer_drop, exception_private::RunResult, types::PyTrait, value::Value};

/// Implementation of the repr() builtin function.
///
/// Returns a string containing a printable representation of an object.
/// `py_repr` already yields a heap `str` `Value`, so it is returned as-is
/// without an intermediate `String` round-trip.
pub fn builtin_repr(vm: &mut VM<'_>, args: ArgValues) -> RunResult<Value> {
    let value = args.get_one_arg("repr", vm.heap)?;
    defer_drop!(value, vm);
    native_work::charge(vm, native_work::text_scan_steps(value, vm))?;
    value.py_repr(vm)
}
