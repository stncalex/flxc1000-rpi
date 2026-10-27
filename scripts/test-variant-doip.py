
import socket
import struct
import argparse

PI_IP = "169.254.16.1"
DOIP_PORT = 13400

TESTER = 0x0E00
ECU = 0x1000


def receive_exact(sock, count):
    data = b""
    while len(data) < count:
        chunk = sock.recv(count - len(data))
        if not chunk:
            raise ConnectionError("Connection closed")
        data += chunk
    return data


def send_doip(sock, payload_type, payload):
    # DoIP protocol version 0x02, inverse 0xFD
    header = struct.pack(
        ">BBHI", 0x02, 0xFD, payload_type, len(payload)
    )
    sock.sendall(header + payload)


def receive_doip(sock):
    header = receive_exact(sock, 8)
    version, inverse, payload_type, length = struct.unpack(
        ">BBHI", header
    )
    if version != 0x02 or inverse != 0xFD:
        raise ValueError("Unexpected DoIP protocol version")
    return payload_type, receive_exact(sock, length)


def uds_request(sock, request):
    payload = struct.pack(">HH", TESTER, ECU) + bytes(request)
    send_doip(sock, 0x8001, payload)

    while True:
        payload_type, response = receive_doip(sock)

        if payload_type == 0x8002:
            print("DoIP ACK:", response.hex(" "))
        elif payload_type == 0x8003:
            raise RuntimeError("DoIP NACK: " + response.hex(" "))
        elif payload_type == 0x8001:
            uds = response[4:]
            print("UDS response:", uds.hex(" "))
            return uds
        else:
            print("Other DoIP message:", hex(payload_type))

def parse_args():
    parser = argparse.ArgumentParser(
        description="FLXC1000 variant coding over UDS/DoIP"
    )

    parser.add_argument(
        "mode",
        choices=["white", "blue", "green", "idle"],
        help="LED mode to select"
    )

    return parser.parse_args()

args = parse_args()

LED_MODES = {
    "white": 0x00,
    "blue": 0x01,
    "green": 0x02,
    "idle": 0x03,
}

selected_mode = LED_MODES[args.mode]

with socket.create_connection((PI_IP, DOIP_PORT), timeout=5) as sock:
    sock.settimeout(5)

    # Routing activation
    activation = struct.pack(">HBI", TESTER, 0x00, 0)
    send_doip(sock, 0x0005, activation)

    payload_type, response = receive_doip(sock)
    print("Routing activation:", hex(payload_type), response.hex(" "))

    if payload_type != 0x0006 or len(response) < 5 or response[4] != 0x10:
        raise RuntimeError("Routing activation failed")

    # Enter extended session
    uds_request(sock, [0x10, 0x03])

    # Read initial variant
    uds_request(sock, [0x22, 0xF2, 0x01])

    # Select the LED mode specified by the user
    print(f"Selecting LED mode: {args.mode}")
    uds_request(sock, [0x2E, 0xF2, 0x01, selected_mode])

    # Read back
    uds_request(sock, [0x22, 0xF2, 0x01])
