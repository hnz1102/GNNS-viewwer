// GPIO control state -- stub (GPIO control removed)
// This file is kept for compilation compatibility only.
use std::sync::{Arc, Mutex};

#[allow(dead_code)]
pub const GPIO_PIN_MAP: [u8; 6] = [4, 5, 6, 7, 8, 9];

#[derive(Clone)]
pub struct GpioPwmState {
    _dummy: Arc<Mutex<()>>,
}

impl GpioPwmState {
    pub fn new() -> Self {
        Self { _dummy: Arc::new(Mutex::new(())) }
    }
    pub fn get_gpio(&self) -> [bool; 6] { [false; 6] }
    pub fn set_gpio(&self, _index: usize, _value: bool) {}
}
