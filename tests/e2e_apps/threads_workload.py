import threading

WORKERS = 4
CALLS_PER_WORKER = 250
THREAD_NAME = "e2e-worker"


def worker_marker() -> int:
    return 1


def run_worker() -> None:
    for _ in range(CALLS_PER_WORKER):
        worker_marker()


def main() -> None:
    workers = [
        threading.Thread(target=run_worker, name=f"{THREAD_NAME}-{i}")
        for i in range(WORKERS)
    ]
    for worker in workers:
        worker.start()
    for worker in workers:
        worker.join()


if __name__ == "__main__":
    main()
