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
	"io/fs"
)

type file struct {
	fs.FileInfo

	root  string
	read  func(p []byte) (n int, err error)
	write func(p []byte) (n int, err error)
	close func() error
}

func (f file) Stat() (fs.FileInfo, error) { return f.FileInfo, nil }

func (f file) Read(b []byte) (int, error) { return f.read(b) }

func (f file) ReadDir(_ int) ([]fs.DirEntry, error) { return nil, fs.ErrInvalid }

func (f file) Type() fs.FileMode { return f.Mode() }

func (f file) Info() (fs.FileInfo, error) { return f.FileInfo, nil }

func (f file) Write(p []byte) (n int, err error) { return f.write(p) }

func (f file) Root() string { return f.root }

func (f file) Close() error {
	if f.close != nil {
		return f.close()
	}
	return nil
}
