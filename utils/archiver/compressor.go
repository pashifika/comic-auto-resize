// Package archiver
/*
 * Version: 1.0.0
 * Copyright (c) 2022. Pashifika
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */
package archiver

import (
	"github.com/pashifika/compress"
	"github.com/pashifika/util/files"
)

func (f *FileSystem) SaveToDisk() error {
	out, err := files.FileOpen(f.savePath, "w")
	if err != nil {
		return err
	}
	entries := make([]compress.ArchiverFile, len(f.archivers))
	for i, af := range f.archivers {
		af.RenameExt(f.Cache.ImageExtension())
		entries[i] = af
	}
	//ext := strings.TrimLeft(filepath.Ext(f.savePath), ".")
	// other formats is not supported
	return f.compressor.CreateArchiverFile("zip", out, entries)
}
