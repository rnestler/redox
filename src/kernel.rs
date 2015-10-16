#![crate_type="staticlib"]
#![feature(alloc)]
#![feature(asm)]
#![feature(box_syntax)]
#![feature(core_intrinsics)]
#![feature(core_simd)]
#![feature(core_slice_ext)]
#![feature(core_str_ext)]
#![feature(fnbox)]
#![feature(fundamental)]
#![feature(lang_items)]
#![feature(no_std)]
#![feature(unboxed_closures)]
#![feature(unsafe_no_drop_flag)]
#![feature(unwind_attributes)]
#![no_std]

extern crate alloc;

use alloc::boxed::Box;

use core::{mem, ptr};

use common::context::*;
use common::debug;
use common::event::{self, Event, EventOption};
use common::memory;
use common::paging::*;
use common::queue::Queue;
use common::resource::URL;
use common::scheduler;
use common::string::{String, ToString};
use common::time::Duration;
use common::vec::Vec;

use drivers::pci::*;
use drivers::pio::*;
use drivers::ps2::*;
use drivers::rtc::*;
use drivers::serial::*;

pub use externs::*;

use graphics::bmp::*;
use graphics::color::Color;
use graphics::display::{self, Display};
use graphics::point::Point;

use programs::package::*;
use programs::scheme::*;
use programs::session::*;

use schemes::arp::*;
use schemes::context::*;
use schemes::debug::*;
use schemes::ethernet::*;
use schemes::icmp::*;
use schemes::ip::*;
use schemes::memory::*;
use schemes::random::*;
use schemes::tcp::*;
use schemes::time::*;
use schemes::window::*;

use syscall::handle::*;

#[path="audio/src/lib.rs"]
mod audio;

#[path="common/src/lib.rs"]
mod common;

#[path="drivers/src/lib.rs"]
mod drivers;

pub mod externs;

#[path="graphics/src/lib.rs"]
mod graphics;

#[path="network/src/lib.rs"]
mod network;

#[path="programs/src/lib.rs"]
mod programs;

#[path="schemes/src/lib.rs"]
mod schemes;

#[path="syscall/src/lib.rs"]
mod syscall;

#[path="usb/src/lib.rs"]
mod usb;

static mut debug_display: *mut Box<Display> = 0 as *mut Box<Display>;
static mut debug_point: Point = Point { x: 0, y: 0 };
static mut debug_draw: bool = false;
static mut debug_redraw: bool = false;
static mut debug_command: *mut String = 0 as *mut String;

static mut clock_realtime: Duration = Duration {
    secs: 0,
    nanos: 0
};

static mut clock_monotonic: Duration = Duration {
    secs: 0,
    nanos: 0
};

static PIT_DURATION: Duration = Duration {
    secs: 0,
    nanos: 2250286
};

static mut session_ptr: *mut Box<Session> = 0 as *mut Box<Session>;

static mut events_ptr: *mut Queue<Event> = 0 as *mut Queue<Event>;

unsafe fn idle_loop() -> ! {
    loop {
        asm!("cli");

        let mut halt = true;

        let contexts = & *contexts_ptr;
        for i in 1..contexts.len() {
            match contexts.get(i) {
                Some(context) => if context.interrupted {
                    halt = false;
                    break;
                },
                None => ()
            }
        }

        if halt {
            asm!("sti");
            asm!("hlt");
        } else {
            asm!("sti");
        }

        context_switch(true);
    }
}

unsafe fn poll_loop() -> ! {
    let session = &mut *session_ptr;

    loop {
        session.on_poll();

        context_switch(false);
    }
}

unsafe fn event_loop() -> ! {
    let session = &mut *session_ptr;
    let events = &mut *events_ptr;
    let mut cmd = String::new();
    loop {
        loop {
            let reenable = scheduler::start_no_ints();

            let event_option = events.pop();

            scheduler::end_no_ints(reenable);

            match event_option {
                Some(event) => {
                    if debug_draw {
                        match event.to_option() {
                            EventOption::Key(key_event) => {
                                if key_event.pressed {
                                    match key_event.scancode {
                                        event::K_F2 => {
                                            ::debug_draw = false;
                                            (*::session_ptr).redraw = true;
                                        },
                                        event::K_BKSP => if cmd.len() > 0 {
                                            debug::db(8);
                                            cmd.vec.pop();
                                        },
                                        _ => match key_event.character {
                                            '\0' => (),
                                            '\n' => {
                                                let reenable = scheduler::start_no_ints();
                                                *::debug_command = cmd + '\n';
                                                scheduler::end_no_ints(reenable);

                                                cmd = String::new();
                                                debug::dl();
                                            }
                                            _ => {
                                                cmd.vec.push(key_event.character);
                                                debug::dc(key_event.character);
                                            }
                                        }
                                    }
                                }
                            },
                            _ => ()
                        }
                    } else {
                        if event.code == 'k' && event.b as u8 == event::K_F1 && event.c > 0 {
                            ::debug_draw = true;
                            ::debug_redraw = true;
                        } else {
                            session.event(event);
                        }
                    }
                },
                None => break
            }
        }

        context_switch(false);
    }
}

unsafe fn redraw_loop() -> ! {
    let session = &mut *session_ptr;

    loop {
        if debug_draw {
            let display = &*(*debug_display);
            if debug_redraw {
                debug_redraw = false;
                display.flip();
            }
        } else {
            session.redraw();
        }

        context_switch(false);
    }
}

pub unsafe fn debug_init() {
    PIO8::new(0x3F8 + 1).write(0x00);
    PIO8::new(0x3F8 + 3).write(0x80);
    PIO8::new(0x3F8 + 0).write(0x03);
    PIO8::new(0x3F8 + 1).write(0x00);
    PIO8::new(0x3F8 + 3).write(0x03);
    PIO8::new(0x3F8 + 2).write(0xC7);
    PIO8::new(0x3F8 + 4).write(0x0B);
    PIO8::new(0x3F8 + 1).write(0x01);
}

unsafe fn init(font_data: usize) {
    scheduler::start_no_ints();

    debug_display = 0 as *mut Box<Display>;
    debug_point = Point { x: 0, y: 0 };
    debug_draw = false;
    debug_redraw = false;

    clock_realtime.secs = 0;
    clock_realtime.nanos = 0;

    clock_monotonic.secs = 0;
    clock_monotonic.nanos = 0;

    contexts_ptr = 0 as *mut Vec<Box<Context>>;
    context_i = 0;
    context_enabled = false;

    session_ptr = 0 as *mut Box<Session>;

    events_ptr = 0 as *mut Queue<Event>;

    debug_init();

    debug::d("Test\n");

    Page::init();
    memory::cluster_init();
    //Unmap first page to catch null pointer errors (after reading memory map)
    Page::new(0).unmap();

    ptr::write(display::FONTS, font_data);

    debug_display = memory::alloc_type();
    ptr::write(debug_display, box Display::root());
    (*debug_display).set(Color::new(0, 0, 0));
    debug_draw = true;
    debug_command = memory::alloc_type();
    ptr::write(debug_command, String::new());

    debug::d("Redox ");
    debug::dd(mem::size_of::<usize>() * 8);
    debug::d(" bits ");
    debug::dl();

    clock_realtime = RTC::new().time();

    contexts_ptr = memory::alloc_type();
    ptr::write(contexts_ptr, Vec::new());
    (*contexts_ptr).push(Context::root());

    session_ptr = memory::alloc_type();
    ptr::write(session_ptr, box Session::new());

    events_ptr = memory::alloc_type();
    ptr::write(events_ptr, Queue::new());

    let session = &mut *session_ptr;

    session.items.push(PS2::new());
    session.items.push(Serial::new(0x3F8, 0x4));

    pci_init(session);

    session.items.push(box ContextScheme);
    session.items.push(box DebugScheme);
    session.items.push(box MemoryScheme);
    session.items.push(box RandomScheme);
    session.items.push(box TimeScheme);

    session.items.push(box EthernetScheme);
    session.items.push(box ARPScheme);
    session.items.push(box IPScheme {
        arp: Vec::new()
    });
    session.items.push(box ICMPScheme);
    session.items.push(box TCPScheme);
    session.items.push(box WindowScheme);

    Context::spawn(box move || {
        poll_loop();
    });
    Context::spawn(box move || {
        event_loop();
    });
    Context::spawn(box move || {
        redraw_loop();
    });
    Context::spawn(box move || {
        ARPScheme::reply_loop();
    });
    Context::spawn(box move || {
        ICMPScheme::reply_loop();
    });

    debug::d("Reenabling interrupts\n");

    //Start interrupts
    scheduler::end_no_ints(true);

    //Load cursor before getting out of debug mode
    debug::d("Loading cursor\n");
    if let Some(mut resource) = URL::from_str("file:///ui/cursor.bmp").open() {
        let mut vec: Vec<u8> = Vec::new();
        resource.read_to_end(&mut vec);

        let cursor = BMPFile::from_data(&vec);

        let reenable = scheduler::start_no_ints();
        session.cursor = cursor;
        session.redraw = true;
        scheduler::end_no_ints(reenable);
    }

    debug::d("Loading schemes\n");
    if let Some(mut resource) = URL::from_str("file:///schemes/").open() {
        let mut vec: Vec<u8> = Vec::new();
        resource.read_to_end(&mut vec);

        for folder in String::from_utf8(&vec).split("\n".to_string()) {
            if folder.ends_with("/".to_string()) {
                let scheme_item = SchemeItem::from_url(
                    &folder.substr(0, folder.len() - 1),
                    &URL::from_string(&("file:///schemes/".to_string() + &folder + &folder.substr(0, folder.len() - 1) + ".bin"))
                );

                let reenable = scheduler::start_no_ints();
                session.items.push(scheme_item);
                scheduler::end_no_ints(reenable);
            }
        }
    }

    debug::d("Loading apps\n");
    if let Some(mut resource) = URL::from_str("file:///apps/").open() {
        let mut vec: Vec<u8> = Vec::new();
        resource.read_to_end(&mut vec);

        for folder in String::from_utf8(&vec).split("\n".to_string()) {
            if folder.ends_with("/".to_string()) {
                let package = Package::from_url(&URL::from_string(&("file:///apps/".to_string() + folder)));

                let reenable = scheduler::start_no_ints();
                session.packages.push(package);
                session.redraw = true;
                scheduler::end_no_ints(reenable);
            }
        }
    }

    debug::d("Loading background\n");
    if let Some(mut resource) = URL::from_str("file:///ui/background.bmp").open() {
        let mut vec: Vec<u8> = Vec::new();
        resource.read_to_end(&mut vec);

        let background = BMPFile::from_data(&vec);

        let reenable = scheduler::start_no_ints();
        session.background = background;
        session.redraw = true;
        scheduler::end_no_ints(reenable);
    }

    debug::d("Enabling context switching\n");
    debug_draw = false;
    context_enabled = true;
}

fn dr(reg: &str, value: usize) {
    debug::d(reg);
    debug::d(": ");
    debug::dh(value as usize);
    debug::dl();
}

#[cold]
#[inline(never)]
#[no_mangle]
#[cfg(target_arch = "x86")]
/// Take regs for kernel calls and exceptions
pub unsafe extern "cdecl" fn kernel(interrupt: u32, mut ax: u32, bx: u32, cx: u32, dx: u32, ip: u32, flags: u32, error: u32) -> usize {
    kernel_inner(interrupt as usize, ax as usize, bx as usize, cx as usize, dx as usize, ip as usize, flags as usize, error as usize)
}

#[cold]
#[inline(never)]
#[no_mangle]
#[cfg(target_arch = "x86_64")]
/// Take regs for kernel calls and exceptions
pub unsafe extern "cdecl" fn kernel(interrupt: u64, mut ax: u64, bx: u64, cx: u64, dx: u64, ip: u64, flags: u64, error: u64) -> usize {
    kernel_inner(interrupt as usize, ax as usize, bx as usize, cx as usize, dx as usize, ip as usize, flags as usize, error as usize)
}

#[inline(always)]
pub unsafe fn kernel_inner(interrupt: usize, mut ax: usize, bx: usize, cx: usize, dx: usize, ip: usize, flags: usize, error: usize) -> usize {
    macro_rules! exception {
        ($name:expr) => ({
            debug::d($name);
            debug::dl();

            dr("CONTEXT", context_i);
            dr("FLAGS", flags);
            dr("IP", ip);
            dr("INT", interrupt);
            dr("AX", ax);
            dr("BX", bx);
            dr("CX", cx);
            dr("DX", dx);

            let cr0: usize;
            asm!("mov $0, cr0" : "=r"(cr0) : : : "intel", "volatile");
            dr("CR0", cr0);

            let cr2: usize;
            asm!("mov $0, cr2" : "=r"(cr2) : : : "intel", "volatile");
            dr("CR2", cr2);

            let cr3: usize;
            asm!("mov $0, cr3" : "=r"(cr3) : : : "intel", "volatile");
            dr("CR3", cr3);

            let cr4: usize;
            asm!("mov $0, cr4" : "=r"(cr4) : : : "intel", "volatile");
            dr("CR4", cr4);

            do_sys_exit(-1);
            loop {
                asm!("sti");
                asm!("hlt");
            }
        })
    };

    macro_rules! exception_error {
        ($name:expr) => ({
            debug::d($name);
            debug::dl();

            dr("CONTEXT", context_i);
            dr("FLAGS", error);
            dr("IP", flags);
            dr("ERROR", ip);
            dr("INT", interrupt);
            dr("AX", ax);
            dr("BX", bx);
            dr("CX", cx);
            dr("DX", dx);

            let cr0: usize;
            asm!("mov $0, cr0" : "=r"(cr0) : : : "intel", "volatile");
            dr("CR0", cr0);

            let cr2: usize;
            asm!("mov $0, cr2" : "=r"(cr2) : : : "intel", "volatile");
            dr("CR2", cr2);

            let cr3: usize;
            asm!("mov $0, cr3" : "=r"(cr3) : : : "intel", "volatile");
            dr("CR3", cr3);

            let cr4: usize;
            asm!("mov $0, cr4" : "=r"(cr4) : : : "intel", "volatile");
            dr("CR4", cr4);

            do_sys_exit(-1);
            loop {
                asm!("sti");
                asm!("hlt");
            }
        })
    };

    if interrupt >= 0x20 && interrupt < 0x30 {
        if interrupt >= 0x28 {
            PIO8::new(0xA0).write(0x20);
        }

        PIO8::new(0x20).write(0x20);
    }

    match interrupt {
        0x20 => {
            let reenable = scheduler::start_no_ints();
            clock_realtime = clock_realtime + PIT_DURATION;
            clock_monotonic = clock_monotonic + PIT_DURATION;
            scheduler::end_no_ints(reenable);

            context_switch(true);
        }
        0x21 => (*session_ptr).on_irq(0x1), //keyboard
        0x23 => (*session_ptr).on_irq(0x3), // serial 2 and 4
        0x24 => (*session_ptr).on_irq(0x4), // serial 1 and 3
        0x25 => (*session_ptr).on_irq(0x5), //parallel 2
        0x26 => (*session_ptr).on_irq(0x6), //floppy
        0x27 => (*session_ptr).on_irq(0x7), //parallel 1 or spurious
        0x28 => (*session_ptr).on_irq(0x8), //RTC
        0x29 => (*session_ptr).on_irq(0x9), //pci
        0x2A => (*session_ptr).on_irq(0xA), //pci
        0x2B => (*session_ptr).on_irq(0xB), //pci
        0x2C => (*session_ptr).on_irq(0xC), //mouse
        0x2D => (*session_ptr).on_irq(0xD), //coprocessor
        0x2E => (*session_ptr).on_irq(0xE), //disk
        0x2F => (*session_ptr).on_irq(0xF), //disk
        0x80 => ax = syscall_handle(ax, bx, cx, dx),
        0xFF => {
            init(ax);
            idle_loop();
        }
        0x0 => exception!("Divide by zero exception"),
        0x1 => exception!("Debug exception"),
        0x2 => exception!("Non-maskable interrupt"),
        0x3 => exception!("Breakpoint exception"),
        0x4 => exception!("Overflow exception"),
        0x5 => exception!("Bound range exceeded exception"),
        0x6 => exception!("Invalid opcode exception"),
        0x7 => exception!("Device not available exception"),
        0x8 => exception_error!("Double fault"),
        0xA => exception_error!("Invalid TSS exception"),
        0xB => exception_error!("Segment not present exception"),
        0xC => exception_error!("Stack-segment fault"),
        0xD => exception_error!("General protection fault"),
        0xE => exception_error!("Page fault"),
        0x10 => exception!("x87 floating-point exception"),
        0x11 => exception_error!("Alignment check exception"),
        0x12 => exception!("Machine check exception"),
        0x13 => exception!("SIMD floating-point exception"),
        0x14 => exception!("Virtualization exception"),
        0x1E => exception_error!("Security exception"),
        _ => exception!("Unknown Interrupt"),
    }

    ax
}
