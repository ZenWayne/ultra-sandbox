package main

import (
	"encoding/binary"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"sync"
	"syscall"

	"github.com/creack/pty"
	"golang.org/x/term"
)

// Frame types: client→server
const (
	FrameExec   byte = 0x01
	FrameStdin  byte = 0x02
	FrameResize byte = 0x03
	FrameSignal byte = 0x04
	FrameEOF    byte = 0x05
)

// Frame types: server→client
const (
	FrameStdout byte = 0x11
	FrameStderr byte = 0x12
	FrameExit   byte = 0x13
)

type execRequest struct {
	Cmd  string   `json:"cmd"`
	Args []string `json:"args"`
	Cwd  string   `json:"cwd"`
	TTY  bool     `json:"tty"`
	Rows uint16   `json:"rows"`
	Cols uint16   `json:"cols"`
}

// --- Frame I/O ---

func writeFrame(w io.Writer, ftype byte, data []byte) error {
	buf := make([]byte, 3+len(data))
	buf[0] = ftype
	binary.BigEndian.PutUint16(buf[1:], uint16(len(data)))
	copy(buf[3:], data)
	_, err := w.Write(buf)
	return err
}

func readFrame(r io.Reader) (byte, []byte, error) {
	hdr := make([]byte, 3)
	if _, err := io.ReadFull(r, hdr); err != nil {
		return 0, nil, err
	}
	ftype := hdr[0]
	n := binary.BigEndian.Uint16(hdr[1:])
	data := make([]byte, n)
	if n > 0 {
		if _, err := io.ReadFull(r, data); err != nil {
			return 0, nil, err
		}
	}
	return ftype, data, nil
}

func encodeExit(code int) []byte {
	b := make([]byte, 4)
	binary.BigEndian.PutUint32(b, uint32(int32(code)))
	return b
}

// --- Sandbox dir (one env var controls socket + shim bin dir) ---

func sandboxDir() string {
	if d := os.Getenv("SANDBOX_DIR"); d != "" {
		return d
	}
	return ".ultra_sandbox"
}

func socketPath() string {
	return filepath.Join(sandboxDir(), "daemon.sock")
}

func shimBinDir() string {
	return filepath.Join(sandboxDir(), "bin")
}

// --- Daemon ---

func runDaemon(sockPath string) {
	os.MkdirAll(filepath.Dir(sockPath), 0755)
	os.Remove(sockPath) // remove stale socket

	ln, err := net.Listen("unix", sockPath)
	if err != nil {
		fmt.Fprintf(os.Stderr, "sandbox daemon: listen %s: %v\n", sockPath, err)
		os.Exit(1)
	}
	os.Chmod(sockPath, 0660)
	fmt.Fprintf(os.Stderr, "sandbox daemon: listening on %s\n", sockPath)

	// Clean up socket on exit
	sigs := make(chan os.Signal, 1)
	signal.Notify(sigs, syscall.SIGINT, syscall.SIGTERM)
	go func() {
		<-sigs
		ln.Close()
		os.Remove(sockPath)
		os.Exit(0)
	}()

	for {
		conn, err := ln.Accept()
		if err != nil {
			return
		}
		go handleClient(conn)
	}
}

func handleClient(conn net.Conn) {
	defer conn.Close()

	ftype, data, err := readFrame(conn)
	if err != nil || ftype != FrameExec {
		return
	}

	var req execRequest
	if err := json.Unmarshal(data, &req); err != nil {
		writeFrame(conn, FrameStderr, []byte("sandbox: invalid exec request\n"))
		writeFrame(conn, FrameExit, encodeExit(1))
		return
	}

	if req.TTY {
		handlePTY(conn, req)
	} else {
		handlePipe(conn, req)
	}
}

func handlePipe(conn net.Conn, req execRequest) {
	cmd := exec.Command(req.Cmd, req.Args...)
	cmd.Dir = req.Cwd
	cmd.Env = os.Environ()

	stdinPipe, err := cmd.StdinPipe()
	if err != nil {
		writeFrame(conn, FrameStderr, []byte(err.Error()+"\n"))
		writeFrame(conn, FrameExit, encodeExit(1))
		return
	}
	stdoutPipe, _ := cmd.StdoutPipe()
	stderrPipe, _ := cmd.StderrPipe()

	if err := cmd.Start(); err != nil {
		writeFrame(conn, FrameStderr, []byte(err.Error()+"\n"))
		writeFrame(conn, FrameExit, encodeExit(1))
		return
	}

	var wg sync.WaitGroup
	wg.Add(2)

	go func() {
		defer wg.Done()
		buf := make([]byte, 32*1024)
		for {
			n, err := stdoutPipe.Read(buf)
			if n > 0 {
				writeFrame(conn, FrameStdout, buf[:n])
			}
			if err != nil {
				return
			}
		}
	}()

	go func() {
		defer wg.Done()
		buf := make([]byte, 32*1024)
		for {
			n, err := stderrPipe.Read(buf)
			if n > 0 {
				writeFrame(conn, FrameStderr, buf[:n])
			}
			if err != nil {
				return
			}
		}
	}()

	// Read client frames (stdin + signals)
	for {
		ft, d, err := readFrame(conn)
		if err != nil || ft == FrameEOF {
			stdinPipe.Close()
			break
		}
		switch ft {
		case FrameStdin:
			stdinPipe.Write(d)
		case FrameSignal:
			if len(d) > 0 && cmd.Process != nil {
				cmd.Process.Signal(syscall.Signal(d[0]))
			}
		}
	}

	wg.Wait()
	cmd.Wait()
	code := 0
	if cmd.ProcessState != nil {
		code = cmd.ProcessState.ExitCode()
	}
	writeFrame(conn, FrameExit, encodeExit(code))
}

func handlePTY(conn net.Conn, req execRequest) {
	cmd := exec.Command(req.Cmd, req.Args...)
	cmd.Dir = req.Cwd
	cmd.Env = os.Environ()

	rows, cols := req.Rows, req.Cols
	if rows == 0 {
		rows = 24
	}
	if cols == 0 {
		cols = 80
	}

	ptmx, err := pty.StartWithSize(cmd, &pty.Winsize{Rows: rows, Cols: cols})
	if err != nil {
		writeFrame(conn, FrameStderr, []byte(err.Error()+"\n"))
		writeFrame(conn, FrameExit, encodeExit(1))
		return
	}
	defer ptmx.Close()

	// Forward PTY output → client stdout
	go func() {
		buf := make([]byte, 32*1024)
		for {
			n, err := ptmx.Read(buf)
			if n > 0 {
				writeFrame(conn, FrameStdout, buf[:n])
			}
			if err != nil {
				return
			}
		}
	}()

	// Read client frames
	for {
		ft, d, err := readFrame(conn)
		if err != nil || ft == FrameEOF {
			break
		}
		switch ft {
		case FrameStdin:
			ptmx.Write(d)
		case FrameResize:
			if len(d) == 4 {
				r := binary.BigEndian.Uint16(d[0:])
				c := binary.BigEndian.Uint16(d[2:])
				pty.Setsize(ptmx, &pty.Winsize{Rows: r, Cols: c})
			}
		case FrameSignal:
			if len(d) > 0 && cmd.Process != nil {
				// Send to process group
				syscall.Kill(-cmd.Process.Pid, syscall.Signal(d[0]))
			}
		}
	}

	cmd.Wait()
	code := 0
	if cmd.ProcessState != nil {
		code = cmd.ProcessState.ExitCode()
	}
	writeFrame(conn, FrameExit, encodeExit(code))
}

// --- Client (run) ---

func runClient(sockPath, cmdName string, args []string) {
	conn, err := net.Dial("unix", sockPath)
	if err != nil {
		fmt.Fprintf(os.Stderr, "sandbox: cannot connect to daemon at %s: %v\n", sockPath, err)
		fmt.Fprintf(os.Stderr, "sandbox: start daemon with: sandbox daemon\n")
		os.Exit(1)
	}
	defer conn.Close()

	isTTY := term.IsTerminal(int(os.Stdin.Fd())) && term.IsTerminal(int(os.Stdout.Fd()))
	var rows, cols uint16 = 24, 80
	if isTTY {
		if w, h, err := term.GetSize(int(os.Stdout.Fd())); err == nil {
			rows, cols = uint16(h), uint16(w)
		}
	}

	cwd, _ := os.Getwd()
	req := execRequest{
		Cmd:  cmdName,
		Args: args,
		Cwd:  cwd,
		TTY:  isTTY,
		Rows: rows,
		Cols: cols,
	}
	data, _ := json.Marshal(req)
	if err := writeFrame(conn, FrameExec, data); err != nil {
		fmt.Fprintf(os.Stderr, "sandbox: write error: %v\n", err)
		os.Exit(1)
	}

	// Set raw mode if TTY
	var oldState *term.State
	if isTTY {
		oldState, _ = term.MakeRaw(int(os.Stdin.Fd()))
		defer func() {
			if oldState != nil {
				term.Restore(int(os.Stdin.Fd()), oldState)
			}
		}()

		// Forward SIGWINCH → RESIZE frame
		winch := make(chan os.Signal, 1)
		signal.Notify(winch, syscall.SIGWINCH)
		go func() {
			for range winch {
				if w, h, err := term.GetSize(int(os.Stdout.Fd())); err == nil {
					b := make([]byte, 4)
					binary.BigEndian.PutUint16(b[0:], uint16(h))
					binary.BigEndian.PutUint16(b[2:], uint16(w))
					writeFrame(conn, FrameResize, b)
				}
			}
		}()
	}

	// Forward SIGINT/SIGTERM → SIGNAL frame
	sigs := make(chan os.Signal, 1)
	signal.Notify(sigs, syscall.SIGINT, syscall.SIGTERM)
	go func() {
		for sig := range sigs {
			var num byte
			switch sig {
			case syscall.SIGINT:
				num = byte(syscall.SIGINT)
			case syscall.SIGTERM:
				num = byte(syscall.SIGTERM)
			}
			writeFrame(conn, FrameSignal, []byte{num})
		}
	}()

	exitCode := 0
	done := make(chan struct{})

	// Receive server frames
	go func() {
		defer close(done)
		for {
			ft, d, err := readFrame(conn)
			if err != nil {
				return
			}
			switch ft {
			case FrameStdout:
				os.Stdout.Write(d)
			case FrameStderr:
				os.Stderr.Write(d)
			case FrameExit:
				if len(d) == 4 {
					exitCode = int(int32(binary.BigEndian.Uint32(d)))
				}
				return
			}
		}
	}()

	// Send stdin → STDIN frames
	go func() {
		buf := make([]byte, 32*1024)
		for {
			n, err := os.Stdin.Read(buf)
			if n > 0 {
				writeFrame(conn, FrameStdin, buf[:n])
			}
			if err != nil {
				writeFrame(conn, FrameEOF, nil)
				return
			}
		}
	}()

	<-done

	if oldState != nil {
		term.Restore(int(os.Stdin.Fd()), oldState)
		oldState = nil
	}
	os.Exit(exitCode)
}

// --- Map ---

func runMap(sandboxDir, cmdName string, remove bool) {
	shimPath := filepath.Join(sandboxDir, cmdName)

	if remove {
		if err := os.Remove(shimPath); err != nil {
			fmt.Fprintf(os.Stderr, "sandbox map: remove %s: %v\n", shimPath, err)
			os.Exit(1)
		}
		fmt.Printf("removed shim: %s\n", shimPath)
		return
	}

	shim := "#!/bin/sh\nexec sandbox run " + cmdName + " \"$@\"\n"
	if err := os.WriteFile(shimPath, []byte(shim), 0755); err != nil {
		fmt.Fprintf(os.Stderr, "sandbox map: write %s: %v\n", shimPath, err)
		os.Exit(1)
	}
	fmt.Printf("mapped: %s -> sandbox run %s\n", shimPath, cmdName)
}

// --- Main ---

func usage() {
	fmt.Fprintln(os.Stderr, `usage:
  sandbox daemon [--socket PATH]     start host daemon
  sandbox run <cmd> [args...]        run command via daemon
  sandbox map <cmd> [--remove]       create/remove shim in $SANDBOX_DIR/bin/ (default .ultra_sandbox/bin/)`)
	os.Exit(1)
}

func main() {
	if len(os.Args) < 2 {
		usage()
	}

	switch os.Args[1] {
	case "daemon":
		sockPath := socketPath()
		for i := 2; i < len(os.Args)-1; i++ {
			if os.Args[i] == "--socket" {
				sockPath = os.Args[i+1]
			}
		}
		runDaemon(sockPath)

	case "run":
		if len(os.Args) < 3 {
			fmt.Fprintln(os.Stderr, "usage: sandbox run <cmd> [args...]")
			os.Exit(1)
		}
		runClient(socketPath(), os.Args[2], os.Args[3:])

	case "map":
		if len(os.Args) < 3 {
			fmt.Fprintln(os.Stderr, "usage: sandbox map <cmd> [--remove]")
			os.Exit(1)
		}
		cmdName := os.Args[2]
		remove := len(os.Args) > 3 && os.Args[3] == "--remove"
		binDir := shimBinDir()
		os.MkdirAll(binDir, 0755)
		runMap(binDir, cmdName, remove)

	default:
		// Allow argv[0] as command name (symlink invocation)
		self := filepath.Base(os.Args[0])
		if self != "sandbox" {
			runClient(socketPath(), self, os.Args[1:])
			return
		}
		usage()
	}
}

