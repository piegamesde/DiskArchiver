
use std::process::Command;
use std::str;
use nom::IResult;
use nom::error::VerboseError;
use nom::multi::many0;
use nom::sequence::tuple;
use nom::character::complete::multispace0;
use nom::character::complete::char as char_tag;
use nom::sequence::terminated;
use nom::bytes::complete::take_until;
use nom::sequence::preceded;
use nom::bytes::complete::tag;
use std::fs;
use std::io;
use std::io::Read;
use std::io::Write;
use cursive::Cursive;
use cursive::views::TextView;
use cursive::views::Dialog;
use cursive::align::HAlign;
use cursive::view::Scrollable;
use cursive::views::LinearLayout;
use cursive::traits::*;
use cursive::views::ProgressBar;
use cursive::views::Checkbox;
use cursive::views::ListView;
use cursive::views::EditView;
use cursive::views::Button;
use std::thread;
use std::time::Duration;
use cursive::utils::Counter;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use cursive::event::Event;
use std::sync::Mutex;
use std::path::Path;
use std::io::Seek;
use std::io::SeekFrom;

extern crate tempfile_fast;

// Depends on the following being installed;
//  libdvdcss - driver to decode DVDs
//  lsscsi    - to discover disk drives.
//  blkid     - to discover if disks are in drives.
//  eject     - optional, to open and close drives.
//  cdrtools  - for the `isoinfo` command.

// If you keep getting IO errors, you may need to set your computer's DVD region.

enum DiskInfoError {
    LaunchFail,   // Failed to launch application. No permission, out of memory, not installed, something else?
    ConvertToUTF, // Application output was not valid UTF8.
    Parse,        // Failed to parse the output of the application.
}

#[derive(Clone)]
enum DriveStatus {
    Setup,
    NoDisk,
    Copying,
    WaitingForName,
    ConfirmingName,
    Saving(String),
    Done,

    CopyWriteError(String),
    CopyReadError(String),
    NonFatalCopyWriteError(String),
    NonFatalCopyReadError(String),
    IsoFetchError,
}

struct DiskDrive {
    file: String,
    has_disk: AtomicBool,
    status_message: Mutex<DriveStatus>,
}

#[derive(Clone)]
struct ISOInfo {
    name: String,
    block_size: usize,
    length: usize,
}

enum CopyError {
    Read(String),
    Write(String),
    None
}

pub type ParserResult<'a, O> = IResult<&'a str, O, VerboseError<&'a str>>;

fn parse_disk_drive_list(input: &str) -> ParserResult<Vec<Arc<DiskDrive>>> {
    let (input, lines) = many0(
            terminated(take_until("\n"), char_tag('\n'))
    )(input)?;

    let mut drives = Vec::new();

    for line in lines {
        let result: ParserResult<(&str, &str, &str, &str)> = tuple((
            preceded(char_tag('['), terminated(take_until("]"), char_tag(']'))),
            multispace0,
            terminated(take_until(" "), multispace0),
            take_until("/")
        ))(line);

        match result {
            Ok(result) => {
                let (name, result) = result;
                let (_, _, drive_type, _) = result;

                if drive_type == "cd/dvd" {
                    let len = name.len();

                    let mut drive = DiskDrive {
                        file: String::from(name),
                        has_disk: AtomicBool::new(false),
                        status_message: Mutex::new(DriveStatus::Setup),
                    };
                    drive.file.remove(len - 1);

                    drives.push(Arc::new(drive));
                }
            },
            _ => {} // Ignore invalid lines.
        }
    }

    Ok((input, drives))
}

fn get_drive_status_message_string(status: &DriveStatus) -> String {
    let message = match status {
        DriveStatus::Setup => String::from("Setting up..."),
        DriveStatus::NoDisk => String::from("No Disk."),
        DriveStatus::Copying => String::from("Copying..."),
        DriveStatus::WaitingForName | DriveStatus::ConfirmingName => String::from("Check the \"Settings ready\" box to finish."),
        DriveStatus::Saving(_) => String::from("Saving..."),
        DriveStatus::Done => String::from("Done."),

        DriveStatus::CopyReadError(message) => format!("Error reading disk: {}", message),
        DriveStatus::CopyWriteError(message) => format!("Error writing to output file: {}", message),
        DriveStatus::NonFatalCopyWriteError(message) => format!("Non fatal error reading disk: {}", message),
        DriveStatus::NonFatalCopyReadError(message) => format!("Non fatal error writing to output file: {}", message),
        DriveStatus::IsoFetchError => String::from("Failed to fetch ISO data from disk drive. Is the isoinfo command installed?"),
    };

    message
}

fn list_disk_drives() -> Result<Vec<Arc<DiskDrive>>, DiskInfoError> {
    let mut command = Command::new("lsscsi");
    let output = command.output().map_err(|_| { DiskInfoError::LaunchFail })?;

    let data = str::from_utf8(&output.stdout).map_err(|_| { DiskInfoError::ConvertToUTF })?;

    Ok(parse_disk_drive_list(data).map_err(|_| { DiskInfoError::Parse })?.1)
}

fn parse_bulk_id_list(input: &str) -> ParserResult<Vec<(&str, &str)>> {
    many0(
        tuple((
            terminated(take_until(":"), char_tag(':')),
            terminated(take_until("\n"), char_tag('\n'))
        ))
    )(input)
}

fn check_disks_in_drives(drives: &Vec<Arc<DiskDrive>>) -> Result<(), DiskInfoError> {
    let mut command = Command::new("blkid");
    let output = command.output().map_err(|_| { DiskInfoError::LaunchFail })?;

    let data = str::from_utf8(&output.stdout).map_err(|_| { DiskInfoError::ConvertToUTF })?;

    let (_, disks) = parse_bulk_id_list(data).map_err(|_| { DiskInfoError::Parse })?;

    for drive in drives.iter() {
        drive.has_disk.swap(disks.iter().find(|e| drive.file.starts_with(e.0)).is_some(), Relaxed);
    }

    Ok(())
}

fn parse_iso_info(input: &str) -> ParserResult<ISOInfo> {
    let (input, _) = take_until("CD-ROM is")(input)?;                                          // Skip to the format
    let (input, _) = terminated(take_until("\n"), char_tag('\n'))(input)?;                     // Format
    let (input, _) = terminated(take_until("\n"), char_tag('\n'))(input)?;                     // System id
    let (input, volume_id_line) = terminated(take_until("\n"), char_tag('\n'))(input)?;  // Volume id
    let (input, _) = terminated(take_until("\n"), char_tag('\n'))(input)?;                     // Volume set id
    let (input, _) = terminated(take_until("\n"), char_tag('\n'))(input)?;                     // Publisher id
    let (input, _) = terminated(take_until("\n"), char_tag('\n'))(input)?;                     // Data preparer id
    let (input, _) = terminated(take_until("\n"), char_tag('\n'))(input)?;                     // Application id
    let (input, _) = terminated(take_until("\n"), char_tag('\n'))(input)?;                     // Copyright File id
    let (input, _) = terminated(take_until("\n"), char_tag('\n'))(input)?;                     // Abstract File id
    let (input, _) = terminated(take_until("\n"), char_tag('\n'))(input)?;                     // Bibliographic File id
    let (input, _) = terminated(take_until("\n"), char_tag('\n'))(input)?;                     // Volume set size
    let (input, _) = terminated(take_until("\n"), char_tag('\n'))(input)?;                     // Volume set sequence number
    let (input, block_size_line) = terminated(take_until("\n"), char_tag('\n'))(input)?; // Logical block size
    let (_, number_of_blocks_line) = terminated(take_until("\n"), char_tag('\n'))(input)?;     // Volume size (in blocks)
    // No other information is important to us.

    // Fine parse the data.
    let (volume_id, _) = tag("Volume id: ")(volume_id_line)?;

    let (block_size, _) = tag("Logical block size is: ")(block_size_line)?;
    let block_size: usize = block_size.parse().unwrap(); // Only way it could panic is if it exceeds the machine's bit width.

    let (number_of_blocks, _) = tag("Volume size is: ")(number_of_blocks_line)?;
    let number_of_blocks: usize = number_of_blocks.parse().unwrap(); // Only way it could panic is if it exceeds the machine's bit width.

    // Ship out the data.
    Ok((input, ISOInfo {
        name: String::from(volume_id),
        block_size,
        length: number_of_blocks * block_size
    }))
}

fn fetch_iso_info(drive: &str) -> Result<ISOInfo, DiskInfoError> {

    let mut command = Command::new("isoinfo");

    command.args(&["-d", &format!("-i{}", drive)]);

    let output = command.output().map_err(|_| { DiskInfoError::LaunchFail })?;

    let data = str::from_utf8(&output.stdout).map_err(|_| { DiskInfoError::ConvertToUTF })?;

    let (_, result) = parse_iso_info(data).map_err(|_| { DiskInfoError::Parse })?;

    Ok(result)
}

fn copy_disk_to_iso<O, CB, ECB>(source: &str, target: &mut O, length: usize, buffer_len: usize, mut callback: CB, mut error_callback: ECB)
    -> Result<(), CopyError> where
    O: Write,
    CB: FnMut(usize),
    ECB: FnMut(CopyError)
{

    // For testing just dumbly return. Creates a lot of compiler warnings but saves hours waiting for disks to copy.
    // return Ok(());

    let mut buffer = vec![0; buffer_len];

    let source_file = fs::File::open(source).unwrap(); // FIXME replace unwrap with a passed error.
    let mut source_file = source_file.take(length as u64);
    let mut position = 0;

    loop {
        let len = match source_file.read(&mut buffer) {
            Ok(0) => {
                break;
            },
            Ok(len) => {
                callback(len);
                error_callback(CopyError::None);
                Ok(len)
            },
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {
                continue;
            },
            Err(error) => {
                error_callback(CopyError::Read(format!("{}", error)));
                // Err(CopyError::Read) // FIXME Try re-opening the device to see if you can recover.
                let mut new_source = fs::File::open(source).unwrap(); // FIXME replace unwrap with a passed error.
                new_source.seek(SeekFrom::Start(position as u64)).map_err( |e| { CopyError::Read(format!("{}", e)) } )?;
                source_file = new_source.take(length as u64);

                continue;
            }
        }?;

        position += len;

        target.write_all(&buffer[..len]).map_err(|e| {
            CopyError::Write(format!("{}", e))
        })?;
    }

    Ok(())
}

fn eject_drive_disk(drive: &str) -> Result<bool, DiskInfoError> {
    fn attempt_eject(drive: &str) -> Result<bool, DiskInfoError> {
        let mut command = Command::new("eject");
        command.arg(drive);

        let status = command.output().map_err(|_| { DiskInfoError::LaunchFail })?.status;

        Ok(status.success())
    }

    // Set as though we failed.
    let mut worked = false;

    for _ in 0..5 {
        if attempt_eject(drive)? {
            // We got it opened!
            worked = true;
            break;
        }
    }

    // Report if we got it opened.
    Ok(worked)
}

fn close_drive_disk(drive: &str) -> Result<bool, DiskInfoError> {
    fn attempt_close(drive: &str) -> Result<bool, DiskInfoError> {
        let mut command = Command::new("eject");
        command.arg("-t");
        command.arg(drive);

        let status = command.output().map_err(|_| { DiskInfoError::LaunchFail })?.status;

        Ok(status.success())
    }

    // Set as though we failed.
    let mut worked = false;

    for _ in 0..5 {
        if attempt_close(drive)? {
            // We got it closed!
            worked = true;
            break;
        }
    }

    // Report if we got it closed.
    Ok(worked)
}

fn add_drive_ui_buttons(drive: &DiskDrive, linear: &mut LinearLayout) {

    let drive1 = drive.file.clone();
    let drive2 = drive.file.clone();

    let buttons = LinearLayout::horizontal()
        .child(Button::new("Eject", move |s| {
            if let Ok(worked) = eject_drive_disk(&drive1) {
                if worked {
                    s.add_layer(Dialog::text("Disk ejected.")
                        .button("Ok", |s| { s.pop_layer(); } ));

                    // Break out of this function before we can hit the fail case.
                    return;
                }
            }

            s.add_layer(Dialog::text("Failed to eject disk.")
                .button("Ok", |s| { s.pop_layer(); } ));

            // Failed to eject drive.
        }))
        .child(Button::new("Close", move |s| {

            if let Ok(worked) = close_drive_disk(&drive2) {
                if worked {
                    s.add_layer(Dialog::text("Disk drive closed.")
                        .button("Ok", |s| { s.pop_layer(); } ));

                    // Break out of this function before we can hit the fail case.
                    return;
                }
            }

            s.add_layer(Dialog::text("Failed to close disk drive.")
                .button("Ok", |s| { s.pop_layer(); } ));

            // Failed to close drive.
        }))
        .full_width();
    linear.add_child(buttons);
}

fn add_status_indicator(s: &mut Cursive, drive: &Arc<DiskDrive>, linear: &mut LinearLayout, status_id: &String) {

    let drive = drive.clone();
    let status_id = status_id.clone();

    linear.add_child(TextView::new("----").with_id(&status_id));
    s.add_global_callback(Event::Refresh, move |s| {
        // Shouldn't fail since we made this.
        let mut status = s.find_id::<TextView>(&status_id).unwrap();

        status.set_content(get_drive_status_message_string(&drive.status_message.lock().unwrap()));
    });
}

fn spawn_drive_thread(s: &mut Cursive, drive: &Arc<DiskDrive>, counter: Counter, name_id: &str, ready_id: &str) {
    let drive = drive.clone();

    let cb = s.cb_sink().clone();

    let name_id = String::from(name_id);
    let ready_id = String::from(ready_id);

    thread::spawn(move || {
        loop {
            // Wait for a disk

            *drive.status_message.lock().unwrap() = DriveStatus::NoDisk;

            while !drive.has_disk.load(Relaxed) {
                thread::sleep(Duration::from_millis(5000));
            }

            if let Ok(info) = fetch_iso_info(&drive.file) {
                *drive.status_message.lock().unwrap() = DriveStatus::Copying;

                let name_id = name_id.clone();
                let ready_id = ready_id.clone();

                let default_iso_name = format!("{}.iso", info.name);

                cb.send(Box::new(move |s| {
                    let mut text_box = s.find_id::<EditView>(&name_id).unwrap();
                    let mut ready_checkbox = s.find_id::<Checkbox>(&ready_id).unwrap();

                    ready_checkbox.set_checked(false);
                    text_box.set_content(default_iso_name);
                })).unwrap();

                let mut target = tempfile_fast::PersistableTempFile::new_in("./").unwrap();
                // let mut target = fs::OpenOptions::new().write(true).create(true).open(format!("{}.iso", info.name)).unwrap();

                counter.set(0);

                let mut progress: usize = 0;
                let length = info.length as f64;

                match copy_disk_to_iso(&drive.file, &mut target, info.length, info.block_size, |read| {
                    progress += read;
                    counter.set((((progress as f64) / length) * 1000.0) as usize);
                },
                |error| {
                    // Called when there's a non-fatal error.
                    *drive.status_message.lock().unwrap() = match error {
                        CopyError::Read(err) => DriveStatus::NonFatalCopyReadError(err),
                        CopyError::Write(err) => DriveStatus::NonFatalCopyWriteError(err),
                        CopyError::None => DriveStatus::Copying,
                    };
                }) {
                    Ok(()) => {
                        *drive.status_message.lock().unwrap() = DriveStatus::WaitingForName;

                        // Wait for name.
                        loop {
                            let status = drive.status_message.lock().unwrap().clone();

                            match status {

                                DriveStatus::Saving(name) => { // We have the name! Save it!
                                    target.persist_by_rename(name).unwrap();
                                    break;
                                }

                                _=> { // Wait.
                                    thread::sleep(Duration::from_millis(5000));
                                }
                            }
                        }

                        *drive.status_message.lock().unwrap() = DriveStatus::Done;
                    },
                    Err(error) => {
                        *drive.status_message.lock().unwrap() = match error {
                            CopyError::Read(err) => DriveStatus::CopyReadError(err),
                            CopyError::Write(err) => DriveStatus::CopyWriteError(err),
                            CopyError::None => DriveStatus::Copying, // Should never happen.
                        };
                    }
                }
            } else {
                *drive.status_message.lock().unwrap() = DriveStatus::IsoFetchError;
            }

            // Wait for disk to be removed.
            while drive.has_disk.load(Relaxed) {
                thread::sleep(Duration::from_millis(5000));
            }
        }
    });
}

fn add_name_settings(s: &mut Cursive, linear: &mut LinearLayout, name_id: &str, ready_id: &str, drive: &Arc<DiskDrive>) {
    let settings = ListView::new()
        .child("Settings ready: ", Checkbox::new().with_id(ready_id))
        .child("File name: ", EditView::new().with_id(name_id));
    linear.add_child(settings);

    let name_id = String::from(name_id);
    let ready_id = String::from(ready_id);
    let drive = drive.clone();

    s.add_global_callback(Event::Refresh, move |s| {

        let mut status = drive.status_message.lock().unwrap();

        let mut text_box = s.find_id::<EditView>(&name_id).unwrap();
        let ready_checkbox = s.find_id::<Checkbox>(&ready_id).unwrap();

        match *status {
            DriveStatus::WaitingForName => {
                // Only go through with save if box is checked.
                if ready_checkbox.is_checked() {

                    let path = text_box.get_content().clone();

                    if Path::new(path.as_ref()).exists() {
                        // Path exists. Check if they really want to overwrite it.

                        let ready_id1 = ready_id.clone();

                        let drive1 = drive.clone();
                        let drive2 = drive.clone();

                        s.add_layer(Dialog::text("A file with this name exists. Do you want to overwrite it?")
                            .title("Confirm Overwrite")
                            .h_align(HAlign::Center)
                            .button("No", move |s| {
                                s.pop_layer();

                                let mut ready_checkbox = s.find_id::<Checkbox>(&ready_id1).unwrap();
                                ready_checkbox.set_checked(false);

                                // Go back to waiting for a name.
                                let mut status = drive1.status_message.lock().unwrap();
                                *status = DriveStatus::WaitingForName;
                            })
                            .button("Yes", move |s| {
                                s.pop_layer();

                                // Okay, save it.
                                let mut status = drive2.status_message.lock().unwrap();
                                *status = DriveStatus::Saving(path.as_ref().clone());
                            })
                        );

                        // We are now confirming the name. This is needed to prevent infinite spawning of confirmation windows.
                        *status = DriveStatus::ConfirmingName;
                    } else {
                        // No problem just save it.
                        *status = DriveStatus::Saving(path.as_ref().clone());
                    }
                }
            }
            _ => {} // Ignore all other cases.
        }

        // Do not permit editing while we are set as ready.
        text_box.set_enabled(!ready_checkbox.is_checked());
    });
}

fn build_main_menu(s: &mut Cursive, drives: &Vec<Arc<DiskDrive>>) {
    let mut root_view = LinearLayout::vertical();

    for drive in drives.iter() {

        // Build drive UI.
        let mut linear = LinearLayout::vertical();

        let progress_id = format!("progress-{}", drive.file);
        let counter = Counter::new(0);
        let view = ProgressBar::new().max(1000).with_value(counter.clone()).with_id(&progress_id);
        linear.add_child(view);

        let name_id = format!("name-{}", drive.file);
        let ready_id = format!("ready-{}", drive.file);

        add_name_settings(s, &mut linear, &name_id, &ready_id, &drive);

        add_drive_ui_buttons(drive, &mut linear);

        let status_id = format!("status-{}", drive.file);

        add_status_indicator(s, drive, &mut linear, &status_id);

        spawn_drive_thread(s, &drive, counter, &name_id, &ready_id);

        // Now add that to the scrollable list.
        root_view.add_child(Dialog::around(linear).title(&format!("Drive: {}", drive.file)));
    }

    s.add_fullscreen_layer(Dialog::around(root_view.full_width()).title("All Disk Drives").scrollable());
    s.set_autorefresh(true);

    let drives = drives.clone();

    thread::spawn(move || {
        loop {
            if !check_disks_in_drives(&drives).is_ok() {
                // TODO something.
            }
            thread::sleep(Duration::from_millis(5000));
        }
    });
}

fn main() {

    let mut siv = Cursive::default();

    siv.add_global_callback(cursive::event::Key::Esc, |s| {
        s.add_layer(
            Dialog::text("Are you sure you want to quit?")
                .h_align(HAlign::Center)
                .button("No", |s| { s.pop_layer(); })
                .button("Yes", |s| s.quit())
        );
    });

    let drives = list_disk_drives();

    match drives {
        Ok(drives) => {
            let drives = Arc::new(drives);

            let mut intro_text = format!("Press <esc> at any time to quit.\nFound {} disk drives.\n", drives.len());
            for drive in drives.iter() {
                intro_text += &format!("{}\n", drive.file);
            }

            siv.add_layer(
                Dialog::text(intro_text)
                    .title("Mass Disk Archiver")
                    .h_align(HAlign::Center)
                    .button("Continue", move |s| {
                        s.pop_layer();

                        build_main_menu(s, &drives);
                    })
            );
        },
        Err(error) => {
            let message = match error {
                DiskInfoError::LaunchFail =>
                    "Failed to launch lsscsi. Is it not installed?",
                DiskInfoError::ConvertToUTF =>
                    "Failed to convert lsscsi output to UTF8 for parsing. Major bug?",
                DiskInfoError::Parse =>
                    "Failed to parse lsscsi output. Has the application changed its formatting?",
            };

            siv.add_layer(
                Dialog::text(message)
                    .title("Mass Disk Archiver")
                    .button("Exit", |s| s.quit())
            );
        }
    }

    siv.run();
}

#[cfg(test)]
mod test;
