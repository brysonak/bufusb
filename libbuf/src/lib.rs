/*
    bufusb - Tool for flashing USB drives across platforms
    Copyright (C) 2026 Bryson Kelly

    This program is free software: you can redistribute it and/or modify
    it under the terms of the GNU General Public License as published by
    the Free Software Foundation, either version 3 of the License, or
    (at your option) any later version.

    This program is distributed in the hope that it will be useful,
    but WITHOUT ANY WARRANTY; without even the implied warranty of
    MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
    GNU General Public License for more details.

    You should have received a copy of the GNU General Public License
    along with this program.  If not, see <https://www.gnu.org/licenses/>.
 */


pub mod copy;
pub mod list;
pub mod logger;
pub mod mode;
pub mod privilege;
pub mod validate;
pub mod writer;

pub use list::{list_drives, print_device_table, UsbDevice};
pub use logger::init as init_logger;
pub use mode::{ImageCaps, Mode};
pub use privilege::{elevate_or_warn, is_privileged};
pub use validate::{device_size, validate, WriteParams};
pub use writer::write;
