//! Code signing in process with apple-codesign (rcodesign's library).
//!
//! Everything the installer signs is signed ad hoc: the CS_ADHOC flag, an empty
//! requirement set and the empty CMS wrapper, SHA-256 only, which is what
//! vphone-cli's `.appleAdHoc` style writes. TXM routes a CodeDirectory without
//! the flag into a branch demanding a CMS signature (see the comment in the
//! original's `patchMachO`).

use crate::bail;
use crate::error::{Error, Result};
use apple_codesign::{
    MachFile, MachOSigner, SettingsScope, SigningSettings, UnifiedSigner,
};
use plist::Value;
use std::fs;
use std::path::Path;

fn err(e: impl std::fmt::Display) -> Error {
    Error::Sign(e.to_string())
}

/// How to sign.
#[derive(Clone, Debug, Default)]
pub struct SignOptions {
    /// The CodeDirectory identifier; the file's name when `None`.
    pub identifier: Option<String>,
    /// Entitlements XML to embed.
    pub entitlements: Option<Vec<u8>>,
    /// Merge over the entitlements the file is already signed with.
    pub merge_existing: bool,
}

/// Merge `new` over `base` (both XML plist dictionaries): each of its keys set
/// where it stands, appended where it does not.
pub fn merge_entitlements(base: &[u8], new: &[u8]) -> Result<Vec<u8>> {
    let parse = |d: &[u8]| -> Result<plist::Dictionary> {
        if d.is_empty() {
            return Ok(plist::Dictionary::new());
        }
        match Value::from_reader_xml(d)? {
            Value::Dictionary(dict) => Ok(dict),
            _ => bail!("entitlements are not a dictionary"),
        }
    };
    let mut merged = parse(base)?;
    for (k, v) in parse(new)? {
        merged.insert(k, v);
    }
    let mut out = Vec::new();
    plist::to_writer_xml(&mut out, &Value::Dictionary(merged))?;
    Ok(out)
}

/// The CodeDirectory identifier of a signed Mach-O (all slices must agree).
pub fn identifier_of(data: &[u8]) -> Result<Option<String>> {
    let file = MachFile::parse(data).map_err(err)?;
    let mut found: Option<String> = None;
    for macho in file.iter_macho() {
        let Some(signature) = macho.code_signature().map_err(err)? else { continue };
        let Some(cd) = signature.code_directory().map_err(err)? else { continue };
        let ident = cd.ident.to_string();
        match &found {
            Some(prior) if *prior != ident => bail!("the slices have different CodeDirectory identifiers"),
            _ => found = Some(ident),
        }
    }
    Ok(found)
}

/// The entitlements XML of each slice carrying any, in slice order.
pub fn entitlements_of(data: &[u8]) -> Result<Vec<Vec<u8>>> {
    let file = MachFile::parse(data).map_err(err)?;
    let mut out = Vec::new();
    for macho in file.iter_macho() {
        let Some(signature) = macho.code_signature().map_err(err)? else { continue };
        if let Some(blob) = signature.entitlements().map_err(err)? {
            out.push(blob.as_str().as_bytes().to_vec());
        }
    }
    Ok(out)
}

/// Sign a Mach-O image, returning the signed bytes.
pub fn sign_data(data: &[u8], options: &SignOptions, default_identifier: &str) -> Result<Vec<u8>> {
    let identifier = options.identifier.as_deref().unwrap_or(default_identifier);
    let entitlements = if options.merge_existing {
        let existing = entitlements_of(data)?.into_iter().find(|e| !e.is_empty()).unwrap_or_default();
        match &options.entitlements {
            Some(new) => Some(merge_entitlements(&existing, new)?),
            None if existing.is_empty() => None,
            None => Some(existing),
        }
    } else {
        options.entitlements.clone()
    };
    let signer = MachOSigner::new(data).map_err(err)?;
    let mut settings = SigningSettings::default();
    settings.set_binary_identifier(SettingsScope::Main, identifier);
    if let Some(xml) = entitlements.filter(|e| !e.is_empty()) {
        let text = String::from_utf8(xml).map_err(err)?;
        settings
            .set_entitlements_xml(SettingsScope::Main, text)
            .map_err(err)?;
    }
    // Bind an embedded __info_plist as the signature's Info slot.
    let file = MachFile::parse(data).map_err(err)?;
    if let Some(info) = file.nth_macho(0).map_err(err)?.embedded_info_plist().map_err(err)? {
        settings.set_info_plist_data(SettingsScope::Main, info);
    }
    let mut out = Vec::with_capacity(data.len() + 64 * 1024);
    signer.write_signed_binary(&settings, &mut out).map_err(err)?;
    Ok(out)
}

/// Sign a file in place, keeping its mode; an interrupted run leaves the original.
pub fn sign_file(path: &Path, options: &SignOptions) -> Result<()> {
    let data = fs::read(path).map_err(|e| Error::path(path, e))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| Error::msg(format!("{}: no file name", path.display())))?;
    let signed = sign_data(&data, options, name)?;
    let tmp = path.with_file_name(format!(".{name}.vphonesign"));
    fs::write(&tmp, signed).map_err(|e| Error::path(&tmp, e))?;
    let mode = fs::metadata(path).map_err(|e| Error::path(path, e))?.permissions();
    fs::set_permissions(&tmp, mode).map_err(|e| Error::path(&tmp, e))?;
    fs::rename(&tmp, path).map_err(|e| Error::path(path, e))
}

/// `rcodesign sign --shallow`: re-seal a bundle in place.
pub fn seal_bundle(bundle: &Path) -> Result<()> {
    let mut settings = SigningSettings::default();
    settings.set_shallow(true);
    UnifiedSigner::new(settings)
        .sign_path_in_place(bundle)
        .map_err(|e| Error::Sign(format!("{}: {e}", bundle.display())))
}
