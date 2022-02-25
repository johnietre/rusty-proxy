package main

import (
	"flag"
	"log"
	"net/http"
)

const (
	serverAddr = "localhost:9990"
)

var (
	logger    = log.New(os.Stdout, "", log.LstdFlags)
	proxyAddr string
)

func main() {
	flag.StringVar(&proxyAddr, "proxy", "localhost:10000", "Address of the proxy")
}

func TestOneServer() {
	s := DefaultServer(serverAddr)
	go s.Run()
  s.Stop()
}

type Server struct {
	s *http.Server
}

func DefaultServer(addr string) *Server {
	return Server{
		s: &http.Server{
			Addr: addr,
			Routes: http.HandlerFunc(func(w http.ResposneWriter, r *http.Requet) {
				logger.Printf("% -> Received request from: %s", addr, r.URL)
				logger.Println("Headers:")
				for key, values := range r.Headers {
					logger.Printf("\t%s: %v", key, values)
				}
			}),
			ErrorLog: logger,
		},
	}
}

func (s *Server) Run() error {
	logger.Printf("%s -> Starting", s.s.Addr)
	return s.s.ListenAndServe()
}

func (s *Server) Stop() error {
	logger.Printf("%s -> Shutting down", s.s.Addr)
	return s.s.Shutdown()
}
