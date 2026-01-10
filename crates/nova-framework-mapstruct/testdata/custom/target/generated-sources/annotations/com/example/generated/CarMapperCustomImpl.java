package com.example.generated;

import com.example.Car;
import com.example.CarDto;
import com.example.CarMapper;

public class CarMapperCustomImpl implements CarMapper {

    @Override
    public CarDto carToCarDto(Car car) {
        if ( car == null ) {
            return null;
        }

        CarDto carDto = new CarDto();
        carDto.setMake( car.getMake() );
        carDto.setSeatCount( car.getNumberOfSeats() );
        return carDto;
    }
}

