package text

import "strings"

// Greeting builds a friendly greeting for name.
func Greeting(name string) string {
	return "hello, " + strings.Trim(name, "/")
}
