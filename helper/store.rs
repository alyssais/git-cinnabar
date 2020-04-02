/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::borrow::Cow;

use bstr::ByteSlice;
use derive_more::{Deref, DerefMut, Display};
use getset::Getters;
use itertools::Itertools;
use percent_encoding::percent_decode;

use crate::libcinnabar::{ensure_notes, get_note_hg, git2hg, hg2git, hg_object_id};
use crate::libgit::{get_note, BlobId, CommitId, RawBlob};
use crate::oid_type;
use crate::util::{FromBytes, SliceExt};

macro_rules! hg2git {
    ($h:ident => $g:ident($i:ident)) => {
        oid_type!($g($i));
        oid_type!($h(hg_object_id));

        impl $h {
            pub fn to_git(&self) -> Option<$g> {
                unsafe {
                    ensure_notes(&mut hg2git);
                    Some($g::from($i::from(
                        get_note_hg(&mut hg2git, &**self).as_ref().cloned()?,
                    )))
                }
            }
        }
    };
}

hg2git!(HgChangesetId => GitChangesetId(CommitId));
hg2git!(HgManifestId => GitManifestId(CommitId));
hg2git!(HgFileId => GitFileId(BlobId));

#[derive(Clone, Deref, Display, Eq, PartialEq, Ord, PartialOrd)]
pub struct GitChangesetMetadataId(BlobId);
#[derive(Clone, Deref, Display, Eq, PartialEq, Ord, PartialOrd)]
pub struct GitFileMetadataId(BlobId);

impl GitChangesetId {
    pub fn to_hg(&self) -> Option<HgChangesetId> {
        //TODO: avoid repeatedly reading metadata for a given changeset.
        //The equivalent python code was keeping a LRU cache.
        let metadata = GitChangesetMetadata::read(self);
        metadata
            .as_ref()
            .and_then(|m| m.parse())
            .map(|m| m.changeset_id().clone())
    }
}

pub struct GitChangesetMetadata(RawBlob);

impl GitChangesetMetadata {
    pub fn read(changeset_id: &GitChangesetId) -> Option<Self> {
        let note = unsafe {
            ensure_notes(&mut git2hg);
            BlobId::from(get_note(&mut git2hg, &***changeset_id).as_ref()?.clone())
        };
        RawBlob::read(&note).map(Self)
    }

    pub fn parse(&self) -> Option<ParsedGitChangesetMetadata> {
        let mut changeset = None;
        let mut manifest = None;
        let mut author = None;
        let mut extra = None;
        let mut files = None;
        let mut patch = None;
        for line in self.0.as_bytes().lines() {
            match line.split2(b' ')? {
                (b"changeset", c) => {
                    changeset = Some(HgChangesetId::from_bytes(c).ok()?)
                }
                (b"manifest", m) => {
                    manifest = Some(HgManifestId::from_bytes(m).ok()?)
                }
                (b"author", a) => author = Some(a),
                (b"extra", e) => extra = Some(e),
                (b"files", f) => files = Some(f),
                (b"patch", p) => patch = Some(p),
                _ => None?,
            }
        }

        Some(ParsedGitChangesetMetadata {
            changeset_id: changeset?,
            manifest_id: manifest.unwrap_or_else(|| HgManifestId::null()),
            author,
            extra,
            files,
            patch,
        })
    }
}

#[derive(Getters)]
pub struct ParsedGitChangesetMetadata<'a> {
    #[getset(get = "pub")]
    changeset_id: HgChangesetId,
    #[getset(get = "pub")]
    manifest_id: HgManifestId,
    author: Option<&'a [u8]>,
    extra: Option<&'a [u8]>,
    files: Option<&'a [u8]>,
    patch: Option<&'a [u8]>,
}

impl<'a> ParsedGitChangesetMetadata<'a> {
    pub fn author(&self) -> Option<&[u8]> {
        self.author.clone()
    }

    pub fn extra(&self) -> Option<ChangesetExtra> {
        self.extra.map(ChangesetExtra::from)
    }

    pub fn files(&self) -> ChangesetFilesIter {
        ChangesetFilesIter(self.files.clone())
    }

    pub fn patch(&self) -> Option<GitChangesetPatch> {
        self.patch.map(GitChangesetPatch)
    }
}

pub struct ChangesetExtra<'a> {
    buf: &'a [u8],
    more: Vec<(&'a [u8], &'a [u8])>,
}

impl<'a> ChangesetExtra<'a> {
    fn from(buf: &'a [u8]) -> Self {
        ChangesetExtra {
            buf,
            more: Vec::new(),
        }
    }

    pub fn new() -> Self {
        ChangesetExtra {
            buf: &b""[..],
            more: Vec::new(),
        }
    }

    pub fn set(&mut self, name: &'a [u8], value: &'a [u8]) {
        for (n, v) in &mut self.more {
            if name == *n {
                *v = value;
                return;
            }
        }
        self.more.push((name, value))
    }

    pub fn dump_into(&self, buf: &mut Vec<u8>) {
        for b in self
            .buf
            .split(|c| *c == b'\0')
            .merge_join_by(&self.more, |e, (n, _v)| {
                e.split2(b':').map(|e| e.0).unwrap_or(e).cmp(n)
            })
            .map(|e| {
                e.map_left(Cow::Borrowed)
                    .map_right(|(n, v)| {
                        let mut buf = Vec::new();
                        buf.extend_from_slice(n);
                        buf.extend_from_slice(&b": "[..]);
                        buf.extend_from_slice(v);
                        Cow::Owned(buf)
                    })
                    .reduce(|_, y| y)
            })
            .intersperse(Cow::Borrowed(&b"\0"[..]))
        {
            buf.extend_from_slice(&b);
        }
    }
}

pub struct ChangesetFilesIter<'a>(Option<&'a [u8]>);

impl<'a> Iterator for ChangesetFilesIter<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<&'a [u8]> {
        let files = self.0.take()?;
        match files.split2(b'\0') {
            Some((a, b)) => {
                self.0 = Some(b);
                Some(a)
            }
            None => Some(files),
        }
    }
}

pub struct GitChangesetPatch<'a>(&'a [u8]);

impl<'a> GitChangesetPatch<'a> {
    pub fn apply(&self, input: &[u8]) -> Option<Vec<u8>> {
        let mut patched = Vec::new();
        let mut last_end = 0;
        for part in self.0.split(|c| *c == b'\0') {
            let (start, end, data) = part.split3(b',')?;
            let start = usize::from_bytes(start).ok()?;
            let data = Cow::from(percent_decode(data));
            patched.extend_from_slice(&input[last_end..start]);
            patched.extend_from_slice(&data);
            last_end = usize::from_bytes(end).ok()?;
        }
        patched.extend_from_slice(&input[last_end..]);
        Some(patched)
    }
}
