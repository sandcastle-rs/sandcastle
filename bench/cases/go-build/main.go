package main

import (
	"encoding/json"
	"fmt"
	"net/http"
	"os"

	"example.com/benchapp/internal/text"
)

func main() {
	http.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		_ = json.NewEncoder(w).Encode(map[string]string{"greeting": text.Greeting(r.URL.Path)})
	})
	if len(os.Args) > 1 && os.Args[1] == "serve" {
		fmt.Println(http.ListenAndServe(":8080", nil))
	}
	fmt.Println(text.Greeting("bench"))
}
