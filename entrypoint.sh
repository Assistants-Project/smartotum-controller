#!/bin/sh

CRASH_COUNT=0

while true;
do
  echo "Starting Smartotum Controller"

  /usr/local/bin/smartotum-controller

  EXIT_CODE=$?
  
  if [ $EXIT_CODE -ne 0 ]; then
    CRASH_COUNT=$((CRASH_COUNT + 1))
    echo "Smartotum Controller crashed ${CRASH_COUNT} times (exit code: ${EXIT_CODE}), restarting in 5 seconds..."
    sleep 5
  else
    echo "Smartotum Controller exited normally"
    break
  fi
done
