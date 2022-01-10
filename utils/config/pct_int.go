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
	"strconv"
)

type pctInt struct {
	Value   int
	Default int
}

func (q *pctInt) UnmarshalFlag(value string) error {
	i, err := strconv.Atoi(value)
	if err != nil {
		return err
	}
	if i >= 100 {
		i = 100
	} else if i <= 0 {
		i = q.Default
	}
	q.Value = i
	return nil
}

func (q pctInt) MarshalFlag() (string, error) {
	return strconv.Itoa(q.Value), nil
}
