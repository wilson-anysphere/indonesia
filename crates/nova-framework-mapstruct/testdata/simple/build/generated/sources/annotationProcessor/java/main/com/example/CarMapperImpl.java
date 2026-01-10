package com.example;

public class CarMapperImpl implements CarMapper {
    @Override
    public CarDto carToCarDto(Car car) {
        if (car == null) {
            return null;
        }

        CarDto dto = new CarDto();
        dto.seatCount = car.numberOfSeats;
        return dto;
    }
}

