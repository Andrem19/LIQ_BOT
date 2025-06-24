import requests
import numpy as np
import traceback
from datetime import datetime
import talib

def convert_timeframe(opens: np.ndarray, highs: np.ndarray, lows: np.ndarray, closes: np.ndarray, timeframe: int, ln: int):
    lenth_opens = len(opens)
    length = lenth_opens // timeframe if ln == 0 else ln

    new_opens = np.zeros(length)
    new_highs = np.zeros(length)
    new_lows = np.zeros(length)
    new_closes = np.zeros(length)

    for i in range(length):
        start = lenth_opens - (i + 1) * timeframe
        end = lenth_opens - i * timeframe

        new_opens[-(i + 1)] = opens[start]
        new_highs[-(i + 1)] = np.max(highs[start:end])
        new_lows[-(i + 1)] = np.min(lows[start:end])
        new_closes[-(i + 1)] = closes[end - 1]

    return new_opens, new_highs, new_lows, new_closes

def get_kline(coin: str, number_candles: int, interv: int):
    try:
        endpoint = f'/fapi/v1/klines?symbol={coin}&interval={interv}m&limit={number_candles}'
        url = 'https://fapi.binance.com' + endpoint
        with requests.Session() as http_session:
            response = http_session.get(url).json()
        new_list = [[x[0], float(x[1]), float(x[2]), float(x[3]), float(x[4]), float(x[5])] for x in response]
        return np.array(new_list)
    except Exception as e:
        print(f"Error [process_kline binance {datetime.now()} coin: {coin}]: {e}")
        print(f"Exception type: {type(e)}")
        print(traceback.format_exc())
        return [[10], [10]]
    
def get_atr(highs, lows, closes, period):

    atr = talib.ATR(highs, lows, closes, period)
    return atr
    
    
def main(): 
    data = get_kline('SOLUSDT', 200, 1)
    
    sample = data
    closes = sample[:, 4].astype(float)
    highs = sample[:, 2].astype(float)
    lows = sample[:, 3].astype(float)
    opens = sample[:, 1].astype(float)
    volumes = sample[:, 5].astype(np.float64)
    
    opens, highs, lows, closes = convert_timeframe(opens, highs, lows, closes, 5, 28)
    
    atr_list = get_atr(highs, lows, closes, 14)
    print(atr_list)
    
    
    
if __name__ == "__main__":
    main()