/*
SPDX-License-Identifier: Apache-2.0 OR MIT

Copyright 2022 The Arboard contributors

The project to which this file belongs is licensed under either of
the Apache 2.0 or the MIT license at the licensee's choice. The terms
and conditions of the chosen license apply to this file.
*/

//! Encoding and decoding of the Windows `CF_HTML` ("HTML Format") clipboard format.
//!
//! The format is a textual header of `Key:Value` lines followed by the HTML document. The header
//! contains byte offsets (counted from the start of the data) of the HTML document and of the
//! fragment that was actually copied.
//!
//! See <https://learn.microsoft.com/en-us/windows/win32/dataxchg/html-clipboard-format>.
//!
//! This lives outside of the Windows backend so that the parsing, which handles untrusted data
//! placed on the clipboard by other applications, can be tested on every platform.

const START_FRAGMENT_COMMENT: &[u8] = b"<!--StartFragment-->";
const END_FRAGMENT_COMMENT: &[u8] = b"<!--EndFragment-->";

/// Wraps an HTML fragment into the `CF_HTML` format.
pub(crate) fn wrap(ctn: &str) -> String {
	let h_version = "Version:0.9";
	let h_start_html = "\r\nStartHTML:";
	let h_end_html = "\r\nEndHTML:";
	let h_start_frag = "\r\nStartFragment:";
	let h_end_frag = "\r\nEndFragment:";
	let c_start_frag = "\r\n<html>\r\n<body>\r\n<!--StartFragment-->\r\n";
	let c_end_frag = "\r\n<!--EndFragment-->\r\n</body>\r\n</html>";
	let h_len = h_version.len()
		+ h_start_html.len()
		+ 10 + h_end_html.len()
		+ 10 + h_start_frag.len()
		+ 10 + h_end_frag.len()
		+ 10;
	let n_start_html = h_len + 2;
	let n_start_frag = h_len + c_start_frag.len();
	let n_end_frag = n_start_frag + ctn.len();
	let n_end_html = n_end_frag + c_end_frag.len();
	format!(
		"{h_version}{h_start_html}{n_start_html:010}{h_end_html}{n_end_html:010}{h_start_frag}{n_start_frag:010}{h_end_frag}{n_end_frag:010}{c_start_frag}{ctn}{c_end_frag}"
	)
}

/// The offsets found in a `CF_HTML` header. Negative or unparsable values are `None`.
#[derive(Default, Debug, PartialEq, Eq)]
struct Header {
	start_html: Option<usize>,
	end_html: Option<usize>,
	start_fragment: Option<usize>,
	end_fragment: Option<usize>,
}

fn parse_offset(value: &[u8]) -> Option<usize> {
	// Negative values (`-1`) are used by the specification to signal an absent offset.
	std::str::from_utf8(value).ok()?.trim().parse::<usize>().ok()
}

fn parse_header(data: &[u8]) -> Header {
	let mut header = Header::default();

	for line in data.split(|&b| b == b'\r' || b == b'\n').filter(|line| !line.is_empty()) {
		// The header ends where the HTML document starts.
		if line.starts_with(b"<") {
			break;
		}

		let Some(separator) = line.iter().position(|&b| b == b':') else {
			break;
		};
		let (key, value) = (&line[..separator], &line[separator + 1..]);

		match key {
			b"StartHTML" => header.start_html = parse_offset(value),
			b"EndHTML" => header.end_html = parse_offset(value),
			b"StartFragment" => header.start_fragment = parse_offset(value),
			b"EndFragment" => header.end_fragment = parse_offset(value),
			// `Version`, `StartSelection`, `EndSelection`, `SourceURL` and any future keys.
			_ => {}
		}
	}

	header
}

/// Returns `data[start..end]` if both offsets are present and describe a valid range.
fn checked_range(data: &[u8], start: Option<usize>, end: Option<usize>) -> Option<&[u8]> {
	match (start, end) {
		(Some(start), Some(end)) if start <= end => data.get(start..end),
		_ => None,
	}
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
	haystack.windows(needle.len()).position(|window| window == needle)
}

/// Extracts the copied HTML fragment from `CF_HTML` clipboard data.
///
/// The data comes from arbitrary applications, so none of the header's offsets are trusted: they
/// are bounds-checked against the data, and if they are unusable the fragment is located using
/// the `<!--StartFragment-->`/`<!--EndFragment-->` comments or, failing that, the whole HTML
/// document is returned.
pub(crate) fn extract_fragment(data: &[u8]) -> Option<&[u8]> {
	// The clipboard allocation can be larger than the data written to it and is usually NULL
	// terminated, so only consider the bytes before the first NULL character.
	let data = match data.iter().position(|&b| b == 0) {
		Some(end) => &data[..end],
		None => data,
	};

	let header = parse_header(data);

	if let Some(fragment) = checked_range(data, header.start_fragment, header.end_fragment) {
		return Some(fragment);
	}

	// The offsets were missing or bogus, so try to find the fragment markers in the document.
	let html = checked_range(data, header.start_html, header.end_html.or(Some(data.len())))
		.or_else(|| find(data, b"<").map(|start| &data[start..]))?;

	if let Some(start) = find(html, START_FRAGMENT_COMMENT) {
		let fragment = &html[start + START_FRAGMENT_COMMENT.len()..];
		if let Some(end) = find(fragment, END_FRAGMENT_COMMENT) {
			return Some(&fragment[..end]);
		}
	}

	Some(html)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn round_trip() {
		for fragment in ["", "<b>hello</b> <i>world</i>!", "Some utf8: 🤓 ∑φ(n)<ε 🐔"] {
			let wrapped = wrap(fragment);
			assert_eq!(extract_fragment(wrapped.as_bytes()), Some(fragment.as_bytes()));
		}
	}

	#[test]
	fn wrapped_offsets_point_at_the_document() {
		let wrapped = wrap("<p>hi</p>");
		let header = parse_header(wrapped.as_bytes());

		let html = &wrapped[header.start_html.unwrap()..header.end_html.unwrap()];
		assert!(html.starts_with("<html>"));
		assert!(html.ends_with("</html>"));
	}

	#[test]
	fn trailing_null_bytes_are_ignored() {
		let mut data = wrap("<p>hi</p>").into_bytes();
		data.extend_from_slice(&[0; 16]);
		assert_eq!(extract_fragment(&data), Some(&b"<p>hi</p>"[..]));
	}

	#[test]
	fn offsets_past_the_end_are_rejected() {
		// A malicious or buggy writer can claim offsets which lie outside of the data.
		let data = b"Version:0.9\r\nStartHTML:0000000105\r\nEndHTML:0000000999\r\nStartFragment:0000000990\r\nEndFragment:0000000999\r\n<html><body><!--StartFragment--><p>ok</p><!--EndFragment--></body></html>";
		assert_eq!(extract_fragment(data), Some(&b"<p>ok</p>"[..]));
	}

	#[test]
	fn reversed_offsets_are_rejected() {
		let data = b"Version:0.9\r\nStartFragment:0000000080\r\nEndFragment:0000000010\r\n<html><body><p>ok</p></body></html>";
		assert_eq!(extract_fragment(data), Some(&b"<html><body><p>ok</p></body></html>"[..]));
	}

	#[test]
	fn negative_offsets_mean_absent() {
		let data = b"Version:1.0\r\nStartHTML:-1\r\nEndHTML:-1\r\nStartFragment:-1\r\nEndFragment:-1\r\n<!--StartFragment--><em>x</em><!--EndFragment-->";
		assert_eq!(extract_fragment(data), Some(&b"<em>x</em>"[..]));
	}

	#[test]
	fn lf_only_header_with_source_url() {
		let document =
			"<html><body><!--StartFragment--><a href=\"x\">y</a><!--EndFragment--></body></html>";
		let header_template = "Version:1.0\nStartHTML:0000000000\nEndHTML:0000000000\nStartFragment:0000000000\nEndFragment:0000000000\nSourceURL:https://example.com/a?b=c\n";
		let start_html = header_template.len();
		let start_fragment = start_html + document.find("<a").unwrap();
		let end_fragment = start_html + document.find("<!--EndFragment").unwrap();
		let end_html = start_html + document.len();
		let data = format!(
			"Version:1.0\nStartHTML:{start_html:010}\nEndHTML:{end_html:010}\nStartFragment:{start_fragment:010}\nEndFragment:{end_fragment:010}\nSourceURL:https://example.com/a?b=c\n{document}"
		);

		assert_eq!(extract_fragment(data.as_bytes()), Some(&b"<a href=\"x\">y</a>"[..]));
	}

	#[test]
	fn garbage_does_not_panic() {
		assert_eq!(extract_fragment(b""), None);
		assert_eq!(extract_fragment(b"\0\0\0"), None);
		assert_eq!(extract_fragment(b"StartFragment:99999999999999999999999"), None);
		assert_eq!(extract_fragment(b"<!--StartFragment-->"), Some(&b"<!--StartFragment-->"[..]));
	}
}
