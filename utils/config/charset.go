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
	"errors"
	"fmt"
	"strings"

	"golang.org/x/text/encoding"
	"golang.org/x/text/encoding/japanese"
	"golang.org/x/text/encoding/korean"
	"golang.org/x/text/encoding/simplifiedchinese"
)

// charset see https://i18ns.com/languagecode.html
type charset []encoding.Encoding

func (c *charset) UnmarshalFlag(value string) error {
	var err error
	if value == "" {
		err = errors.New("charset is empty")
	} else {
		strList := strings.Split(strings.ToLower(value), ",")
		var charList []encoding.Encoding
		checkMap := map[string]bool{}
		for _, str := range strList {
			if _, ok := checkMap[str]; ok {
				// Duplicate deletion process
				continue
			}
			switch str {
			case "ja", "ja-jp":
				charList = append(charList, japanese.ShiftJIS)
			case "zh", "zh-cn":
				charList = append(charList, simplifiedchinese.GB18030)
			case "ko", "ko-kr":
				charList = append(charList, korean.EUCKR)
			default:
				err = fmt.Errorf("%s charset is not supported", str)
				break
			}
			checkMap[str] = true
		}
		if err == nil {
			*c = charList
		}
	}
	return err
}

func (c charset) MarshalFlag() (string, error) {
	var strList []string
	checkMap := map[string]bool{}
	for _, e := range c {
		var str string
		switch e {
		case japanese.ShiftJIS:
			str = "ja"
		case simplifiedchinese.GB18030:
			str = "zh"
		case korean.EUCKR:
			str = "ko"
		default:
			return "", fmt.Errorf("%v is unknown charset", e)
		}
		if _, ok := checkMap[str]; ok {
			// Duplicate deletion process
			continue
		}
		strList = append(strList, str)
		checkMap[str] = true
	}
	return strings.Join(strList, ","), nil
}
