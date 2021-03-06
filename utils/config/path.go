// Package config
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
package config

import (
	"fmt"
	"path/filepath"

	"github.com/pashifika/util/files"
)

type path struct {
	Value       string
	CheckExists bool
}

func (p *path) UnmarshalFlag(value string) error {
	fp, err := filepath.Abs(value)
	if err != nil {
		return err
	}
	if p.CheckExists && files.Exists(fp) {
		return fmt.Errorf("\n  %s is exists", fp)
	}
	p.Value = fp
	return nil
}

func (p path) MarshalFlag() (string, error) {
	return p.Value, nil
}
