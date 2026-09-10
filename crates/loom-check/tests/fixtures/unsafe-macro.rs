#[macro_export]
macro_rules! hidden_unsafe {
    () => { unsafe { core::ptr::read_volatile(&1) } };
}
