use std::{
	borrow::Cow,
	collections::HashSet,
	io::Read,
	path::{Path, PathBuf},
};

use wl_clipboard_rs::{
	copy::{self, Error as CopyError, MimeSource, MimeType, Options, ServeRequests, Source},
	paste::{self, get_contents, Error as PasteError, Seat},
	utils::is_primary_selection_supported,
};

#[cfg(feature = "image-data")]
use super::encode_as_png;
use super::{
	gnome_copied_files_from_uri_list, into_unknown, latin1_to_utf8, paths_from_uri_list,
	paths_to_uri_list, LinuxClipboardKind, WaitConfig, GNOME_COPIED_FILES_MIME, KDE_EXCLUSION_HINT,
	KDE_EXCLUSION_MIME, URI_LIST_MIME,
};
use crate::common::Error;
#[cfg(feature = "image-data")]
use crate::common::ImageData;

#[cfg(feature = "image-data")]
const MIME_PNG: &str = "image/png";

/// The MIME types which plain text is read from, in order of preference, and whether the data is
/// encoded as ISO Latin-1 (instead of UTF-8).
///
/// This is deliberately not `paste::MimeType::Text`, which falls back to *any* textual MIME type
/// (`text/html`, `text/uri-list`, JSON, ...) and would therefore return HTML markup or file URIs as
/// plain text.
const TEXT_MIME_TYPES: &[(&str, bool)] = &[
	("text/plain;charset=utf-8", false),
	("text/plain;charset=UTF-8", false),
	("UTF8_STRING", false),
	// X11-style Latin-1 text, which is commonly offered by Xwayland clients.
	("STRING", true),
	("TEXT", false),
	("text/plain", false),
];

fn select_text_mime_type(offered: &HashSet<String>) -> Option<(&'static str, bool)> {
	TEXT_MIME_TYPES.iter().copied().find(|(mime_type, _)| offered.contains(*mime_type))
}

pub(crate) struct Clipboard {}

impl TryInto<copy::ClipboardType> for LinuxClipboardKind {
	type Error = Error;

	fn try_into(self) -> Result<copy::ClipboardType, Self::Error> {
		match self {
			LinuxClipboardKind::Clipboard => Ok(copy::ClipboardType::Regular),
			LinuxClipboardKind::Primary => Ok(copy::ClipboardType::Primary),
			LinuxClipboardKind::Secondary => Err(Error::ClipboardNotSupported),
		}
	}
}

impl TryInto<paste::ClipboardType> for LinuxClipboardKind {
	type Error = Error;

	fn try_into(self) -> Result<paste::ClipboardType, Self::Error> {
		match self {
			LinuxClipboardKind::Clipboard => Ok(paste::ClipboardType::Regular),
			LinuxClipboardKind::Primary => Ok(paste::ClipboardType::Primary),
			LinuxClipboardKind::Secondary => Err(Error::ClipboardNotSupported),
		}
	}
}

fn add_clipboard_exclusions(exclude_from_history: bool, sources: &mut Vec<MimeSource>) {
	if exclude_from_history {
		sources.push(MimeSource {
			source: Source::Bytes(Box::from(KDE_EXCLUSION_HINT)),
			mime_type: MimeType::Specific(String::from(KDE_EXCLUSION_MIME)),
		});
	}
}

fn options(wait: WaitConfig, selection: LinuxClipboardKind) -> Result<Options, Error> {
	let mut opts = Options::new();
	opts.foreground(matches!(wait, WaitConfig::Forever));
	if matches!(wait, WaitConfig::Once) {
		opts.serve_requests(ServeRequests::Only(1));
	}
	opts.clipboard(selection.try_into()?);
	Ok(opts)
}

fn handle_copy_error(e: copy::Error) -> Error {
	match e {
		CopyError::PrimarySelectionUnsupported => Error::ClipboardNotSupported,
		other => into_unknown(other),
	}
}

fn handle_paste_error(e: paste::Error) -> Error {
	match e {
		PasteError::PrimarySelectionUnsupported => Error::ClipboardNotSupported,
		other => into_unknown(other),
	}
}

fn handle_clipboard_read<T, F: FnOnce(Vec<u8>) -> Result<T, Error>>(
	selection: LinuxClipboardKind,
	mime: paste::MimeType,
	into_requested_data: F,
) -> Result<T, Error> {
	let result = get_contents(selection.try_into()?, Seat::Unspecified, mime);
	match result {
		Ok((mut pipe, _)) => {
			let mut buffer = vec![];
			pipe.read_to_end(&mut buffer).map_err(into_unknown)?;
			into_requested_data(buffer)
		}
		Err(PasteError::ClipboardEmpty) | Err(PasteError::NoMimeType) => {
			Err(Error::ContentNotAvailable)
		}
		Err(err) => Err(handle_paste_error(err)),
	}
}

impl Clipboard {
	pub(crate) fn new() -> Result<Self, Error> {
		// Check if it's possible to communicate with the wayland compositor
		match is_primary_selection_supported() {
			// We don't care if the primary clipboard is supported or not, `wl-clipboard-rs` will fail
			// if not and we don't want to duplicate more of their logic.
			Ok(_) => Ok(Self {}),
			Err(e) => Err(into_unknown(e)),
		}
	}

	pub(crate) fn clear(&mut self, selection: LinuxClipboardKind) -> Result<(), Error> {
		let selection = selection.try_into()?;
		copy::clear(selection, copy::Seat::All).map_err(handle_copy_error)
	}

	pub(crate) fn get_text(&mut self, selection: LinuxClipboardKind) -> Result<String, Error> {
		let offered = match paste::get_mime_types(selection.try_into()?, Seat::Unspecified) {
			Ok(offered) => offered,
			Err(PasteError::ClipboardEmpty) | Err(PasteError::NoMimeType) => {
				return Err(Error::ContentNotAvailable)
			}
			Err(err) => return Err(handle_paste_error(err)),
		};

		let (mime_type, is_latin1) =
			select_text_mime_type(&offered).ok_or(Error::ContentNotAvailable)?;

		handle_clipboard_read(selection, paste::MimeType::Specific(mime_type), |contents| {
			if is_latin1 {
				Ok(latin1_to_utf8(&contents))
			} else {
				String::from_utf8(contents).map_err(|_| Error::ConversionFailure)
			}
		})
	}

	pub(crate) fn set_text(
		&self,
		text: Cow<'_, str>,
		selection: LinuxClipboardKind,
		wait: WaitConfig,
		exclude_from_history: bool,
	) -> Result<(), Error> {
		let opts = options(wait, selection)?;
		let mut sources = Vec::with_capacity(if exclude_from_history { 2 } else { 1 });

		sources.push(MimeSource {
			source: Source::Bytes(text.into_owned().into_bytes().into_boxed_slice()),
			mime_type: MimeType::Text,
		});

		add_clipboard_exclusions(exclude_from_history, &mut sources);

		opts.copy_multi(sources).map_err(handle_copy_error)
	}

	pub(crate) fn get_html(&mut self, selection: LinuxClipboardKind) -> Result<String, Error> {
		handle_clipboard_read(selection, paste::MimeType::Specific("text/html"), |contents| {
			String::from_utf8(contents).map_err(|_| Error::ConversionFailure)
		})
	}

	pub(crate) fn set_html(
		&self,
		html: Cow<'_, str>,
		alt: Option<Cow<'_, str>>,
		selection: LinuxClipboardKind,
		wait: WaitConfig,
		exclude_from_history: bool,
	) -> Result<(), Error> {
		let opts = options(wait, selection)?;
		let mut sources = {
			let cap = [true, alt.is_some(), exclude_from_history]
				.map(|v| usize::from(v as u8))
				.iter()
				.sum();
			Vec::with_capacity(cap)
		};

		if let Some(alt) = alt {
			sources.push(MimeSource {
				source: Source::Bytes(alt.into_owned().into_bytes().into_boxed_slice()),
				mime_type: MimeType::Text,
			});
		}

		sources.push(MimeSource {
			source: Source::Bytes(html.into_owned().into_bytes().into_boxed_slice()),
			mime_type: MimeType::Specific(String::from("text/html")),
		});

		add_clipboard_exclusions(exclude_from_history, &mut sources);

		opts.copy_multi(sources).map_err(handle_copy_error)
	}

	#[cfg(feature = "image-data")]
	pub(crate) fn get_image(
		&mut self,
		selection: LinuxClipboardKind,
	) -> Result<ImageData<'static>, Error> {
		use std::io::Cursor;

		handle_clipboard_read(selection, paste::MimeType::Specific(MIME_PNG), |buffer| {
			let image = image::io::Reader::new(Cursor::new(buffer))
				.with_guessed_format()
				.map_err(|_| Error::ConversionFailure)?
				.decode()
				.map_err(|_| Error::ConversionFailure)?;
			let image = image.into_rgba8();

			Ok(ImageData {
				width: image.width() as usize,
				height: image.height() as usize,
				bytes: image.into_raw().into(),
			})
		})
	}

	#[cfg(feature = "image-data")]
	pub(crate) fn set_image(
		&mut self,
		image: ImageData,
		selection: LinuxClipboardKind,
		wait: WaitConfig,
		exclude_from_history: bool,
	) -> Result<(), Error> {
		let opts = options(wait, selection)?;
		let image = encode_as_png(&image)?;

		let mut sources = Vec::with_capacity(if exclude_from_history { 2 } else { 1 });

		sources.push(MimeSource {
			source: Source::Bytes(image.into()),
			mime_type: MimeType::Specific(String::from(MIME_PNG)),
		});

		add_clipboard_exclusions(exclude_from_history, &mut sources);

		opts.copy_multi(sources).map_err(handle_copy_error)
	}

	pub(crate) fn get_file_list(
		&mut self,
		selection: LinuxClipboardKind,
	) -> Result<Vec<PathBuf>, Error> {
		let read_file_list = |contents| Ok(paths_from_uri_list(contents));
		match handle_clipboard_read(
			selection,
			paste::MimeType::Specific(URI_LIST_MIME),
			read_file_list,
		) {
			Ok(paths) => Ok(paths),
			Err(Error::ContentNotAvailable) => handle_clipboard_read(
				selection,
				paste::MimeType::Specific(GNOME_COPIED_FILES_MIME),
				read_file_list,
			),
			Err(error) => Err(error),
		}
	}

	pub(crate) fn set_file_list(
		&self,
		file_list: &[impl AsRef<Path>],
		selection: LinuxClipboardKind,
		wait: WaitConfig,
		exclude_from_history: bool,
	) -> Result<(), Error> {
		let opts = options(wait, selection)?;
		let files = paths_to_uri_list(file_list)?;
		let gnome_files = gnome_copied_files_from_uri_list(&files);

		let mut sources = Vec::with_capacity(if exclude_from_history { 3 } else { 2 });
		sources.push(MimeSource {
			source: Source::Bytes(files.into_bytes().into_boxed_slice()),
			mime_type: MimeType::Specific(String::from(URI_LIST_MIME)),
		});
		sources.push(MimeSource {
			source: Source::Bytes(gnome_files.into_boxed_slice()),
			mime_type: MimeType::Specific(String::from(GNOME_COPIED_FILES_MIME)),
		});

		add_clipboard_exclusions(exclude_from_history, &mut sources);

		opts.copy_multi(sources).map_err(handle_copy_error)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn offered(mime_types: &[&str]) -> HashSet<String> {
		mime_types.iter().map(|m| m.to_string()).collect()
	}

	#[test]
	fn text_mime_type_selection() {
		// Only non-plain textual formats must not be read as text.
		assert_eq!(select_text_mime_type(&offered(&["text/html"])), None);
		assert_eq!(
			select_text_mime_type(&offered(&[URI_LIST_MIME, GNOME_COPIED_FILES_MIME])),
			None
		);
		assert_eq!(select_text_mime_type(&offered(&["application/json"])), None);

		// UTF-8 is preferred over the other encodings.
		assert_eq!(
			select_text_mime_type(&offered(&["text/plain", "STRING", "text/plain;charset=utf-8"])),
			Some(("text/plain;charset=utf-8", false))
		);
		assert_eq!(
			select_text_mime_type(&offered(&["text/html", "UTF8_STRING"])),
			Some(("UTF8_STRING", false))
		);

		// Latin-1 is only used if nothing better is available.
		assert_eq!(
			select_text_mime_type(&offered(&["STRING", "text/plain"])),
			Some(("STRING", true))
		);
		assert_eq!(select_text_mime_type(&offered(&["text/plain"])), Some(("text/plain", false)));
	}
}
